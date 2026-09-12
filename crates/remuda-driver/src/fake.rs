//! In-process FakeDriver that replays built-in Observations for Node/Web loopback.

use crate::Delegation;
use crate::ProviderKind;
use crate::binary::BinaryPin;
use crate::capabilities::{ADAPTER_VERSION, capability_snapshot};
use crate::driver::{Driver, DriverAck, RunHandle};
use crate::error::{DriverError, DriverResult};
use crate::recipe::{
    EnvAllowlistEntry, EnvAllowlistSource, LaunchAudit, LaunchRecipe, RecipePermission,
    RecipeProvider,
};
use async_trait::async_trait;
use remuda_protocol::{
    ApprovalAuthority, BoolLiteral, Completeness, ContentBlock, ContentStatus, Digest, DriverInput,
    DriverKind, Id, InputDelivery, InstanceId, InstanceSpec, InteractionAnswer, InteractionId,
    Knowledge, LifecyclePayload, LifecycleTopic, MessagePayload, MessagePhase, MessageRole,
    MutationOperation, NativeLifecycle, NativeRef, NativeRequestKey, NodeMutation, Observation,
    ObservationPayload, ObservationSource, RunId, SchemaVersion, Severity, SourceChannel,
    SourceCursor, SourceDelivery, TextBlock, Timestamp, U64,
};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{Mutex, mpsc};

const STUB_DIGEST: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000000";
const STUB_TIME: &str = "2026-09-12T10:00:00.000Z";

/// Replay-only driver. Does not spawn a native process.
pub struct FakeDriver {
    kind: DriverKind,
    state: Mutex<State>,
}

struct State {
    started: bool,
    closed: bool,
    events: Option<mpsc::Sender<Observation>>,
    instance_id: Option<InstanceId>,
    host: Option<remuda_protocol::HostId>,
    seq: u64,
    model: Option<String>,
    effort: Option<String>,
}

impl Default for FakeDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeDriver {
    /// Claude-print stub with the built-in OK fixture.
    pub fn new() -> Self {
        Self {
            kind: DriverKind::ClaudePrint,
            state: Mutex::new(State {
                started: false,
                closed: false,
                events: None,
                instance_id: None,
                host: None,
                seq: 0,
                model: None,
                effort: None,
            }),
        }
    }

    fn stub_pin() -> DriverResult<BinaryPin> {
        Ok(BinaryPin {
            abs_path: "/opt/remuda/fake-claude".into(),
            version: "stub".into(),
            sha256: Digest::try_from(STUB_DIGEST.to_string())?,
        })
    }

    fn stub_recipe(spec: &InstanceSpec) -> DriverResult<LaunchRecipe> {
        let pin = Self::stub_pin()?;
        Ok(LaunchRecipe {
            launch_id: Id::new("launch")?,
            driver: spec.driver,
            binary: pin,
            cwd: spec.cwd.clone(),
            argv: vec![
                "-p".into(),
                "--input-format".into(),
                "stream-json".into(),
                "--output-format".into(),
                "stream-json".into(),
                "--setting-sources".into(),
                "user,project,local".into(),
            ],
            env_allowlist: vec![EnvAllowlistEntry {
                name: "CLAUDE_CONFIG_DIR".into(),
                source: EnvAllowlistSource::NativeHome,
                secret_ref: None,
            }],
            materialized_files: vec![],
            setting_sources: vec!["user".into(), "project".into(), "local".into()],
            session_id: Some("01993ab0-0000-7000-8000-000000000003".into()),
            native_home: "/tmp/remuda-driver/fake-home".into(),
            input_delivery: InputDelivery::Stdio,
            provider: RecipeProvider {
                profile_id: spec.provider_profile.id.clone(),
                kind: ProviderKind::Anthropic,
                base_url: "https://gateway.example".into(),
                delegation: Delegation::None,
                secret_ref: None,
                model_requested: spec
                    .model_id
                    .clone()
                    .unwrap_or_else(|| "passthrough/example-model".into()),
            },
            permission: RecipePermission {
                cli_mode: Some("dontAsk".into()),
                prompts: Some("none".into()),
                extra_flags: vec![],
            },
            technical_debt: vec![crate::recipe::TECH_DEBT_M0_PERM_01.into()],
            audit: LaunchAudit {
                env_names: vec!["CLAUDE_CONFIG_DIR".into()],
                credential_refs: vec![],
                redacted_argv: vec![
                    "-p".into(),
                    "--input-format".into(),
                    "stream-json".into(),
                    "--output-format".into(),
                    "stream-json".into(),
                    "--setting-sources".into(),
                    "user,project,local".into(),
                ],
                settings_digest: None,
                prohibited_options_checked: BoolLiteral,
                approval_authority: ApprovalAuthority::Unknown,
            },
        })
    }

    async fn emit(&self, payload: ObservationPayload) -> DriverResult<()> {
        let (tx, observation) = {
            let mut state = self.state.lock().await;
            let tx = state
                .events
                .clone()
                .ok_or(DriverError::ControlUnavailable)?;
            state.seq += 1;
            let seq = state.seq;
            let instance_id = state
                .instance_id
                .clone()
                .ok_or(DriverError::ControlUnavailable)?;
            let host_id = state.host.clone().ok_or(DriverError::ControlUnavailable)?;
            (
                tx,
                build_observation(self.kind, instance_id, host_id, seq, payload)?,
            )
        };
        tx.send(observation)
            .await
            .map_err(|_| DriverError::ControlUnavailable)?;
        Ok(())
    }
}

#[async_trait]
impl Driver for FakeDriver {
    async fn capabilities(&self) -> DriverResult<remuda_protocol::CapabilitySnapshot> {
        let pin = Self::stub_pin()?;
        Ok(capability_snapshot(self.kind, &pin, U64(1), U64(1))?)
    }

    async fn start(&self, spec: InstanceSpec) -> DriverResult<RunHandle> {
        let recipe = Self::stub_recipe(&spec)?;
        let (tx, rx) = mpsc::channel(32);
        let instance_id = InstanceId::new();
        {
            let mut state = self.state.lock().await;
            if state.started && !state.closed {
                return Err(DriverError::ControlUnavailable);
            }
            state.started = true;
            state.closed = false;
            state.events = Some(tx.clone());
            state.instance_id = Some(instance_id.clone());
            state.host = Some(spec.host.clone());
            state.seq = 0;
        }
        let mut ack = DriverAck::transport_written();
        ack.native_ids.insert(
            "sessionId".into(),
            "01993ab0-0000-7000-8000-000000000003".into(),
        );
        drop(tx);
        // Re-open a sender from state for fixture replay.
        let tx = {
            let state = self.state.lock().await;
            state.events.clone()
        };
        if let Some(tx) = tx {
            let host = spec.host.clone();
            let lifecycle = ObservationPayload::Lifecycle(Box::new(LifecyclePayload::Native(
                Box::new(NativeLifecycle {
                    topic: LifecycleTopic::Session,
                    native_name: "session".into(),
                    native_id: Knowledge::Known {
                        value: "01993ab0-0000-7000-8000-000000000003".into(),
                    },
                    status: Knowledge::Known {
                        value: "ready".into(),
                    },
                    related_ids: BTreeMap::new(),
                    data_ref: None,
                    severity: Severity::Info,
                    affects_completion: false,
                }),
            )));
            let message = ObservationPayload::Message(Box::new(MessagePayload {
                mutation: NodeMutation {
                    node_id: Id::new("obj")?,
                    revision: U64(1),
                    operation: MutationOperation::Open,
                    base_revision: None,
                },
                message_id: Id::new("obj")?,
                role: MessageRole::Assistant,
                phase: MessagePhase::Final,
                blocks: vec![ContentBlock::Text(Box::new(TextBlock {
                    text: "OK".into(),
                }))],
                target_block: None,
                parent_tool_call_id: None,
                native_origin: Knowledge::Unknown {
                    reason: "stub".into(),
                    evidence_event_ids: vec![],
                },
                status: ContentStatus::Complete,
            }));
            for (index, payload) in [lifecycle, message].into_iter().enumerate() {
                let observation = build_observation(
                    spec.driver,
                    instance_id.clone(),
                    host.clone(),
                    (index as u64) + 1,
                    payload,
                )?;
                let _ = tx.send(observation).await;
            }
            let mut state = self.state.lock().await;
            state.seq = 2;
        }
        Ok(RunHandle::new(recipe, ack, rx))
    }

    async fn attach(&self, native_ref: NativeRef) -> DriverResult<DriverAck> {
        let state = self.state.lock().await;
        if state.closed {
            return Err(DriverError::AttachWouldWake);
        }
        if !state.started {
            return Err(DriverError::NativeSessionNotFound);
        }
        match &native_ref.session_id {
            Knowledge::Known { .. } => Ok(DriverAck::transport_written()),
            Knowledge::Unknown { .. } | Knowledge::NotApplicable => {
                Err(DriverError::NativeSessionNotFound)
            }
        }
    }

    async fn send(&self, input: DriverInput) -> DriverResult<DriverAck> {
        match &input {
            DriverInput::Prompt(_) => {}
            DriverInput::Steer(_) => return Err(DriverError::CapabilityUnknown("steer".into())),
            DriverInput::ModelSwitch(switch) => {
                {
                    let mut state = self.state.lock().await;
                    if !state.started || state.closed {
                        return Err(DriverError::ControlUnavailable);
                    }
                    if !switch.model_id.is_empty() {
                        state.model = Some(switch.model_id.clone());
                    }
                    if let Some(effort) = &switch.effort {
                        state.effort = Some(effort.clone());
                    }
                }
                let mut related = BTreeMap::new();
                if !switch.model_id.is_empty() {
                    related.insert("model".into(), switch.model_id.clone());
                }
                if let Some(effort) = &switch.effort {
                    related.insert("effort".into(), effort.clone());
                }
                self.emit(ObservationPayload::Lifecycle(Box::new(
                    LifecyclePayload::Native(Box::new(NativeLifecycle {
                        topic: LifecycleTopic::Session,
                        native_name: "model-switch".into(),
                        native_id: Knowledge::NotApplicable,
                        status: Knowledge::Known {
                            value: format!(
                                "applied model={} effort={}",
                                if switch.model_id.is_empty() {
                                    "-"
                                } else {
                                    switch.model_id.as_str()
                                },
                                switch.effort.as_deref().unwrap_or("-"),
                            ),
                        },
                        related_ids: related,
                        data_ref: None,
                        severity: Severity::Info,
                        affects_completion: false,
                    })),
                )))
                .await?;
                return Ok(DriverAck::transport_written());
            }
        }
        self.emit(ObservationPayload::Message(Box::new(MessagePayload {
            mutation: NodeMutation {
                node_id: Id::new("obj")?,
                revision: U64(1),
                operation: MutationOperation::Append,
                base_revision: Some(U64(1)),
            },
            message_id: Id::new("obj")?,
            role: MessageRole::Assistant,
            phase: MessagePhase::Final,
            blocks: vec![ContentBlock::Text(Box::new(TextBlock {
                text: "OK".into(),
            }))],
            target_block: None,
            parent_tool_call_id: None,
            native_origin: Knowledge::Unknown {
                reason: "stub".into(),
                evidence_event_ids: vec![],
            },
            status: ContentStatus::Complete,
        })))
        .await?;
        Ok(DriverAck::transport_written())
    }

    async fn cancel(&self) -> DriverResult<DriverAck> {
        let state = self.state.lock().await;
        if !state.started || state.closed {
            return Err(DriverError::ControlUnavailable);
        }
        Ok(DriverAck::transport_written())
    }

    async fn respond_interaction(
        &self,
        _id: InteractionId,
        _answer: InteractionAnswer,
    ) -> DriverResult<DriverAck> {
        let state = self.state.lock().await;
        if !state.started || state.closed {
            return Err(DriverError::ControlUnavailable);
        }
        Ok(DriverAck::transport_written())
    }

    async fn close(&self) -> DriverResult<DriverAck> {
        let mut state = self.state.lock().await;
        state.closed = true;
        state.events = None;
        Ok(DriverAck::not_dispatched())
    }

    async fn resume(&self, native_ref: NativeRef) -> DriverResult<RunHandle> {
        match &native_ref.session_id {
            Knowledge::Known { value } if !value.is_empty() => {}
            _ => return Err(DriverError::NativeSessionNotFound),
        }
        let mut spec = stub_instance_spec()?;
        spec.host = native_ref.host_id.clone();
        self.start(spec).await
    }
}

fn stub_instance_spec() -> DriverResult<InstanceSpec> {
    let bytes = include_str!("../tests/fixtures/instance-spec.json");
    serde_json::from_str(bytes).map_err(DriverError::Json)
}

fn build_observation(
    driver: DriverKind,
    instance_id: InstanceId,
    host_id: remuda_protocol::HostId,
    seq: u64,
    payload: ObservationPayload,
) -> DriverResult<Observation> {
    static SEQ: AtomicU64 = AtomicU64::new(1);
    let _ = SEQ.fetch_add(1, Ordering::Relaxed);
    Ok(Observation {
        schema_version: SchemaVersion,
        event_id: remuda_protocol::EventId::new(),
        journal_id: Id::new("obj")?,
        instance_id,
        run_id: Some(RunId::new()),
        host_id,
        process_generation: U64(1),
        run_generation: Some(U64(1)),
        seq: U64(seq),
        observed_at: Timestamp::try_from(STUB_TIME.to_string())?,
        native_at: Knowledge::Unknown {
            reason: "stub".into(),
            evidence_event_ids: vec![],
        },
        source: ObservationSource {
            driver_kind: driver,
            driver_version: "stub".into(),
            adapter_version: ADAPTER_VERSION.into(),
            channel: SourceChannel::Runtime,
            delivery: SourceDelivery::Live,
            native_session_id: Knowledge::Known {
                value: "01993ab0-0000-7000-8000-000000000003".into(),
            },
            native_turn_id: Knowledge::Unknown {
                reason: "stub".into(),
                evidence_event_ids: vec![],
            },
            native_agent_id: Knowledge::NotApplicable,
            native_item_id: Knowledge::Unknown {
                reason: "stub".into(),
                evidence_event_ids: vec![],
            },
            native_event_id: Knowledge::Unknown {
                reason: "stub".into(),
                evidence_event_ids: vec![],
            },
            native_request_id: NativeRequestKey::None,
            source_cursor: SourceCursor::Runtime(Box::new(remuda_protocol::RuntimeCursor {
                ledger_revision: U64(seq),
            })),
        },
        completeness: Completeness::Structured,
        raw_ref: None,
        evidence_event_ids: vec![],
        body: payload,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Driver;

    #[tokio::test]
    async fn fake_driver_replays_builtin_observations() {
        let driver = FakeDriver::new();
        let spec: InstanceSpec =
            serde_json::from_str(include_str!("../tests/fixtures/instance-spec.json")).unwrap();
        let mut handle = driver.start(spec).await.unwrap();
        let first = handle.recv().await.expect("lifecycle");
        assert!(matches!(
            first.body,
            ObservationPayload::Lifecycle(_) | ObservationPayload::Message(_)
        ));
        let json = serde_json::to_string(handle.recipe()).unwrap();
        assert!(!json.contains("sk-"));
        driver.close().await.unwrap();
    }

    #[tokio::test]
    async fn fake_claude_applies_model_switch_with_effort() {
        let driver = FakeDriver::new();
        let spec: InstanceSpec =
            serde_json::from_str(include_str!("../tests/fixtures/instance-spec.json")).unwrap();
        let mut handle = driver.start(spec).await.unwrap();
        let _ = handle.recv().await.expect("session lifecycle");
        let _ = handle.recv().await.expect("fixture message");
        driver
            .send(DriverInput::ModelSwitch(Box::new(
                remuda_protocol::ModelSwitchInput {
                    model_id: "opus".into(),
                    effective: remuda_protocol::ModelEffective::NextTurn,
                    effort: Some("ultracode".into()),
                },
            )))
            .await
            .unwrap();
        let applied = handle.recv().await.expect("model-switch observation");
        let ObservationPayload::Lifecycle(payload) = applied.body else {
            panic!("expected lifecycle, got {:?}", applied.body);
        };
        let LifecyclePayload::Native(native) = payload.as_ref() else {
            panic!("expected native lifecycle");
        };
        assert_eq!(native.native_name, "model-switch");
        assert_eq!(
            native.status,
            Knowledge::Known {
                value: "applied model=opus effort=ultracode".into(),
            }
        );
        let state = driver.state.lock().await;
        assert_eq!(state.model.as_deref(), Some("opus"));
        assert_eq!(state.effort.as_deref(), Some("ultracode"));
    }
}
