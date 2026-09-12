//! Instance-local adapters for the native `remuda-driver` implementations.

use crate::{
    Driver, DriverError, DriverFactory, DriverFuture, DriverLaunch, DriverRegistry, DriverRequest,
    DriverStartFuture, NodeError,
};
use remuda_driver::claude_print::{ClaudePrintDriver, ClaudePrintOptions};
use remuda_driver::{
    BinarySource, ClaudeBgDriver, ClaudeBgOptions, ClaudePtyDriver, ClaudePtyOptions, Delegation,
    Driver as NativeDriver, ProviderHealth, ProviderKind, ProviderProfile,
};
use remuda_protocol::{
    ArgvInputPolicy, BgInputDelivery, CarrierSpec, ClaudeInteractionMode, ClaudePermission,
    ClaudePermissionMode, CompletionScope, ContentBlock, DriverInput, DriverKind,
    HerdrRepresentation, HerdrServer, Id, InputOrigin, InstanceSpec, InteractionAnswer, NativeHome,
    NativeHomeMode, PermissionMode, ProfileRef, PromptInput, PromptMode, PtyBackend, PtyCarrier,
    SchemaVersion, SettingsFormat, SettingsOverlay, TextBlock, U64,
};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

/// Filesystem and binary settings shared by native Claude driver factories.
#[derive(Debug, Clone)]
pub struct NativeDriverConfig {
    /// Node-owned data directory for launch recipes and registered native homes.
    pub data_dir: PathBuf,
    /// Explicit Claude executable; `None` resolves `claude` through `PATH` at launch.
    pub claude_binary: Option<PathBuf>,
    /// Explicit Herdr socket directory for PTY/background attach support.
    pub herdr_socket_dir: Option<PathBuf>,
    /// Explicit Herdr executable for PTY operations.
    pub herdr_binary: Option<PathBuf>,
    /// Named, isolated Herdr session.
    pub herdr_session: String,
    /// Claude print initialize timeout.
    pub print_handshake_timeout: Duration,
    /// Non-secret development environment forwarded to drivers.
    pub extra_env: BTreeMap<String, String>,
}

impl NativeDriverConfig {
    /// Build production-shaped defaults rooted below `data_dir`.
    pub fn new(data_dir: PathBuf) -> Self {
        let mut extra_env = BTreeMap::new();
        if let Ok(script) = std::env::var("FAKE_CLAUDE_SCRIPT") {
            extra_env.insert("FAKE_CLAUDE_SCRIPT".to_owned(), script);
        }
        Self {
            data_dir,
            claude_binary: None,
            herdr_socket_dir: None,
            herdr_binary: None,
            herdr_session: "remuda-node".to_owned(),
            print_handshake_timeout: Duration::from_secs(30),
            extra_env,
        }
    }

    /// Override Claude with an absolute development/test executable.
    #[must_use]
    pub fn with_claude_binary(mut self, binary: PathBuf) -> Self {
        self.claude_binary = Some(binary);
        self
    }
}

/// Register `claude-print`, `claude-pty`, and `claude-bg` as per-instance factories.
pub fn native_driver_registry(config: NativeDriverConfig) -> Result<DriverRegistry, NodeError> {
    let registry = DriverRegistry::default();
    for kind in [
        DriverKind::ClaudePrint,
        DriverKind::ClaudePty,
        DriverKind::ClaudeBg,
    ] {
        registry.register_factory(Arc::new(NativeClaudeFactory {
            kind,
            config: config.clone(),
        }))?;
    }
    Ok(registry)
}

struct NativeClaudeFactory {
    kind: DriverKind,
    config: NativeDriverConfig,
}

impl DriverFactory for NativeClaudeFactory {
    fn kind(&self) -> DriverKind {
        self.kind
    }

    fn build(&self, launch: DriverLaunch) -> Result<Arc<dyn Driver>, DriverError> {
        let instance_dir = self
            .config
            .data_dir
            .join("instances")
            .join(launch.instance.meta.id.as_id().as_str());
        let launch_dir = instance_dir.join("launch");
        let native_home = instance_dir.join("native-home");
        std::fs::create_dir_all(&launch_dir)
            .and_then(|()| std::fs::create_dir_all(&native_home))
            .map_err(|error| DriverError::Failed(error.to_string()))?;
        let profile = provider_profile(&launch)?;
        let spec = instance_spec(&launch, &self.config, &profile)?;
        let binary = self
            .config
            .claude_binary
            .clone()
            .map(BinarySource::Path)
            .unwrap_or_else(|| BinarySource::Command("claude".to_owned()));
        let native: Arc<dyn NativeDriver> = match self.kind {
            DriverKind::ClaudePrint => {
                let mut options = ClaudePrintOptions::new(profile, launch_dir, native_home, binary);
                options.extra_env = self.config.extra_env.clone();
                options.handshake_timeout = self.config.print_handshake_timeout;
                Arc::new(ClaudePrintDriver::new(options))
            }
            DriverKind::ClaudePty => {
                let mut options = ClaudePtyOptions::new(profile, launch_dir, native_home, binary);
                options.extra_env = self.config.extra_env.clone();
                options.session_name = self.config.herdr_session.clone();
                options.socket_dir = self.config.herdr_socket_dir.clone();
                options.herdr_binary = self.config.herdr_binary.clone();
                Arc::new(ClaudePtyDriver::new(options))
            }
            DriverKind::ClaudeBg => {
                let mut options = ClaudeBgOptions::new(profile, launch_dir, native_home, binary);
                options.extra_env = self.config.extra_env.clone();
                options.session_name = self.config.herdr_session.clone();
                options.socket_dir = self.config.herdr_socket_dir.clone();
                options.herdr_binary = self.config.herdr_binary.clone();
                Arc::new(ClaudeBgDriver::new(options))
            }
            other => {
                return Err(DriverError::Unsupported(format!(
                    "native Claude factory cannot build {other:?}"
                )));
            }
        };
        Ok(Arc::new(NativeAdapter {
            kind: self.kind,
            native,
            spec,
        }))
    }
}

struct NativeAdapter {
    kind: DriverKind,
    native: Arc<dyn NativeDriver>,
    spec: InstanceSpec,
}

impl Driver for NativeAdapter {
    fn kind(&self) -> DriverKind {
        self.kind
    }

    fn start(&self) -> DriverStartFuture<'_> {
        Box::pin(async move {
            let handle = self
                .native
                .start(self.spec.clone())
                .await
                .map_err(map_driver_error)?;
            Ok(Some(handle.into_events()))
        })
    }

    fn execute(&self, request: DriverRequest) -> DriverFuture<'_> {
        Box::pin(async move {
            match request {
                DriverRequest::Send { prompt } => {
                    self.native
                        .send(prompt_input(prompt))
                        .await
                        .map_err(map_driver_error)?;
                }
                DriverRequest::Cancel => {
                    self.native.cancel().await.map_err(map_driver_error)?;
                }
                DriverRequest::RespondInteraction {
                    interaction_id,
                    answer,
                } => {
                    let interaction_id = interaction_id
                        .parse()
                        .map_err(|error| DriverError::Failed(format!("interaction id: {error}")))?;
                    let answer: InteractionAnswer =
                        serde_json::from_value(answer).map_err(|error| {
                            DriverError::Failed(format!("interaction answer: {error}"))
                        })?;
                    self.native
                        .respond_interaction(interaction_id, answer)
                        .await
                        .map_err(map_driver_error)?;
                }
                DriverRequest::Close => {
                    self.native.close().await.map_err(map_driver_error)?;
                }
            }
            Ok(Vec::new())
        })
    }
}

fn prompt_input(prompt: String) -> DriverInput {
    DriverInput::Prompt(Box::new(PromptInput {
        mode: PromptMode::NewTurn,
        blocks: vec![ContentBlock::Text(Box::new(TextBlock { text: prompt }))],
        origin: InputOrigin::Human,
        native_client_message_id: uuid::Uuid::now_v7().to_string(),
    }))
}

fn provider_profile(launch: &DriverLaunch) -> Result<ProviderProfile, DriverError> {
    let profile_id = Id::new("pvp").map_err(|error| DriverError::Failed(error.to_string()))?;
    let model = if launch.request.model.trim().is_empty() {
        "default".to_owned()
    } else {
        launch.request.model.clone()
    };
    Ok(ProviderProfile {
        id: profile_id,
        kind: ProviderKind::Anthropic,
        base_url: String::new(),
        delegation: Delegation::None,
        secret_ref: None,
        models: vec![model],
        health: ProviderHealth::Healthy,
    })
}

fn instance_spec(
    launch: &DriverLaunch,
    config: &NativeDriverConfig,
    profile: &ProviderProfile,
) -> Result<InstanceSpec, DriverError> {
    let object_id = || Id::new("obj").map_err(|error| DriverError::Failed(error.to_string()));
    let mode = match launch.request.permission_mode.as_str() {
        "auto" => ClaudePermissionMode::Auto,
        "acceptEdits" | "accept-edits" => ClaudePermissionMode::AcceptEdits,
        "dontAsk" | "dont-ask" => ClaudePermissionMode::DontAsk,
        "plan" => ClaudePermissionMode::Plan,
        "bypassPermissions" | "bypass-permissions" => ClaudePermissionMode::BypassPermissions,
        _ => ClaudePermissionMode::Manual,
    };
    let interaction = if launch.request.driver == DriverKind::ClaudePty {
        ClaudeInteractionMode::NativeTty
    } else {
        ClaudeInteractionMode::Host
    };
    let carrier = match launch.request.driver {
        DriverKind::ClaudePty => CarrierSpec::Pty(Box::new(PtyCarrier {
            backend: PtyBackend::Herdr,
            server: HerdrServer {
                binary_path: config
                    .herdr_binary
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "herdr".to_owned()),
                version: "runtime-probed".to_owned(),
                digest: format!("sha256:{:064x}", 0).try_into().map_err(
                    |error: remuda_protocol::WireValueError| DriverError::Failed(error.to_string()),
                )?,
                protocol_version: "runtime-probed".to_owned(),
                server_identity: object_id()?,
                server_epoch: Id::new("epoch")
                    .map_err(|error| DriverError::Failed(error.to_string()))?,
                representation: HerdrRepresentation::RenderedAnsi,
            },
            session: config.herdr_session.clone(),
        })),
        DriverKind::ClaudeBg => CarrierSpec::ClaudeBg(Box::new(remuda_protocol::ClaudeBgCarrier {
            input_delivery: BgInputDelivery::DeferredArgv,
            argv_input_policy: ArgvInputPolicy::ExplicitNonSecret,
        })),
        _ => CarrierSpec::Stdio,
    };
    let model_id = if launch.request.model.trim().is_empty() {
        None
    } else {
        Some(launch.request.model.clone())
    };
    Ok(InstanceSpec {
        schema_version: SchemaVersion,
        host: launch.instance.host_id.clone(),
        workspace_id: launch.instance.workspace_id.clone(),
        kind: launch.request.kind,
        driver: launch.request.driver,
        binary_ref: object_id()?,
        cwd: launch.workspace_root.to_string_lossy().into_owned(),
        worktree: None,
        provider_profile: ProfileRef {
            id: profile.id.clone(),
            revision: U64(1),
        },
        model_id,
        permission_mode: PermissionMode::Claude(Box::new(ClaudePermission { mode, interaction })),
        env: BTreeMap::new(),
        args: Vec::new(),
        settings_overlay: SettingsOverlay {
            format: SettingsFormat::None,
            object_ref: None,
            revision: U64(1),
        },
        native_home: NativeHome {
            mode: NativeHomeMode::Registered,
            store_id: object_id()?,
        },
        carrier,
        required_capabilities: Vec::new(),
        completion_scope: CompletionScope::NativeTurn,
        parent: None,
    })
}

fn map_driver_error(error: remuda_driver::DriverError) -> DriverError {
    DriverError::Failed(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::fixture_instance;
    use remuda_protocol::{AgentKind, HostId, InstanceId, WorkspaceId};

    #[test]
    fn registry_constructs_all_three_native_claude_drivers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = native_driver_registry(NativeDriverConfig::new(dir.path().to_path_buf()))
            .expect("registry");
        for kind in [
            DriverKind::ClaudePrint,
            DriverKind::ClaudePty,
            DriverKind::ClaudeBg,
        ] {
            let instance =
                fixture_instance(InstanceId::new(), HostId::new(), WorkspaceId::new(), kind)
                    .expect("instance");
            let request = crate::CreateInstanceRequest {
                instance_id: Some(instance.meta.id.clone()),
                host_id: Some(instance.host_id.clone()),
                workspace_id: Some(instance.workspace_id.clone()),
                kind: AgentKind::Claude,
                driver: kind,
                model: "haiku".to_owned(),
                provider_profile_id: "native".to_owned(),
                permission_mode: "manual".to_owned(),
                prompt: String::new(),
            };
            let driver = registry
                .build(
                    kind,
                    DriverLaunch {
                        instance,
                        request,
                        workspace_root: dir.path().to_path_buf(),
                    },
                )
                .expect("build driver");
            assert_eq!(driver.kind(), kind);
        }
    }
}
