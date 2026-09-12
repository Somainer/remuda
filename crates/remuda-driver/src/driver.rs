//! In-process [`Driver`] contract and [`RunHandle`] observation stream.

use crate::error::DriverResult;
use crate::recipe::LaunchRecipe;
use async_trait::async_trait;
use remuda_protocol::{
    CommandId, DispatchState, DriverInput, Id, InstanceId, InstanceSpec, InteractionAnswer,
    InteractionId, NativeRef, Observation, RunId, U64,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use tokio::sync::mpsc;

/// Per-call fence carried by Node; `protocol.md` §3.1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallContext {
    /// Command that authorized this call.
    pub command_id: CommandId,
    /// Target instance.
    pub instance_id: InstanceId,
    /// Active run, when the call addresses one.
    pub run_id: Option<RunId>,
    /// Owner fence.
    pub owner_fence: U64,
    /// Process generation.
    pub process_generation: U64,
    /// Run generation, when applicable.
    pub run_generation: Option<U64>,
}

/// Dispatch acknowledgement; `protocol.md` §3.1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriverAck {
    /// How far native dispatch progressed.
    pub dispatch: DispatchState,
    /// Native identifiers observed so far (session, job, pane, …).
    pub native_ids: BTreeMap<String, String>,
    /// Raw object ids captured as evidence.
    pub evidence_raw_ids: Vec<Id>,
}

impl DriverAck {
    /// Transport accepted bytes; does not imply native-input accepted.
    pub fn transport_written() -> Self {
        Self {
            dispatch: DispatchState::TransportWritten,
            native_ids: BTreeMap::new(),
            evidence_raw_ids: vec![],
        }
    }

    /// No native process was started (agy prepared, or control-only).
    pub fn not_dispatched() -> Self {
        Self {
            dispatch: DispatchState::NotDispatched,
            native_ids: BTreeMap::new(),
            evidence_raw_ids: vec![],
        }
    }
}

/// Handle returned by [`Driver::start`] / [`Driver::resume`].
///
/// Exposes the observation stream as `remuda_protocol::Observation`.
#[derive(Debug)]
pub struct RunHandle {
    recipe: LaunchRecipe,
    ack: DriverAck,
    events: mpsc::Receiver<Observation>,
}

impl RunHandle {
    /// Construct a handle around a recipe and observation receiver.
    pub fn new(recipe: LaunchRecipe, ack: DriverAck, events: mpsc::Receiver<Observation>) -> Self {
        Self {
            recipe,
            ack,
            events,
        }
    }

    /// Durable recipe for this run. Contains no secrets or prompts.
    pub fn recipe(&self) -> &LaunchRecipe {
        &self.recipe
    }

    /// Start/resume acknowledgement.
    pub fn ack(&self) -> &DriverAck {
        &self.ack
    }

    /// Next observation, or `None` when the driver closes the stream.
    pub async fn recv(&mut self) -> Option<Observation> {
        self.events.recv().await
    }

    /// Take the raw receiver.
    pub fn into_events(self) -> mpsc::Receiver<Observation> {
        self.events
    }
}

/// Per-instance native driver. `protocol.md` §3.1, M0-07.
///
/// `start` / `attach` / `resume` only establish the connection; they do not send a prompt.
#[async_trait]
pub trait Driver: Send + Sync {
    /// Capability snapshot for this driver and binary generation.
    async fn capabilities(&self) -> DriverResult<remuda_protocol::CapabilitySnapshot>;

    /// Materialize and start. Returns a handle whose event stream is Observations.
    async fn start(&self, spec: InstanceSpec) -> DriverResult<RunHandle>;

    /// Attach to a live native session. Must not wake a stopped job.
    async fn attach(&self, native_ref: NativeRef) -> DriverResult<DriverAck>;

    /// Deliver prompt, steer, or model-switch input.
    async fn send(&self, input: DriverInput) -> DriverResult<DriverAck>;

    /// Request cancellation of the active Run.
    async fn cancel(&self) -> DriverResult<DriverAck>;

    /// Answer a pending Interaction.
    async fn respond_interaction(
        &self,
        id: InteractionId,
        answer: InteractionAnswer,
    ) -> DriverResult<DriverAck>;

    /// Terminate the managed process; keep the native session.
    async fn close(&self) -> DriverResult<DriverAck>;

    /// Resume an explicit [`NativeRef`]. Never `--continue`.
    async fn resume(&self, native_ref: NativeRef) -> DriverResult<RunHandle>;
}
