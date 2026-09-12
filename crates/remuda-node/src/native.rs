//! Instance-local adapters for the native `remuda-driver` implementations.

use crate::{
    Driver, DriverError, DriverFactory, DriverFuture, DriverLaunch, DriverRegistry, DriverRequest,
    DriverStartFuture, NodeError,
};
use remuda_driver::claude_print::{ClaudePrintDriver, ClaudePrintOptions};
use remuda_driver::{
    BinarySource, ClaudeBgDriver, ClaudeBgOptions, ClaudePtyDriver, ClaudePtyOptions, Delegation,
    Driver as NativeDriver, GenericPtyDriver, GenericPtyOptions, ProviderHealth, ProviderKind,
    ProviderProfile, preset_by_id,
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
    /// Explicit registered Claude config directory; otherwise each Instance gets an empty one.
    pub claude_native_home: Option<PathBuf>,
    /// Let claude-print inherit the host user's default Claude login/config.
    pub claude_print_inherit_default_config: bool,
    /// Explicit Herdr socket directory for PTY/background attach support.
    pub herdr_socket_dir: Option<PathBuf>,
    /// Explicit Herdr executable for PTY operations.
    pub herdr_binary: Option<PathBuf>,
    /// Named, isolated Herdr session.
    pub herdr_session: String,
    /// Close unknown panes on startup; disable only for deliberate manual recovery.
    pub herdr_orphan_sweep: bool,
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
            claude_binary: std::env::var_os("REMUDA_CLAUDE_BIN")
                .filter(|path| !path.is_empty())
                .map(PathBuf::from),
            claude_native_home: std::env::var_os("REMUDA_CLAUDE_CONFIG_DIR")
                .filter(|path| !path.is_empty())
                .map(PathBuf::from),
            claude_print_inherit_default_config: std::env::var(
                "REMUDA_CLAUDE_INHERIT_DEFAULT_CONFIG",
            )
            .is_ok_and(|value| matches!(value.as_str(), "1" | "true")),
            herdr_socket_dir: None,
            herdr_binary: std::env::var_os("REMUDA_HERDR_BIN")
                .filter(|path| !path.is_empty())
                .map(PathBuf::from),
            herdr_session: std::env::var("REMUDA_HERDR_SESSION")
                .unwrap_or_else(|_| "remuda-node".to_owned()),
            herdr_orphan_sweep: !std::env::var("REMUDA_HERDR_ORPHAN_SWEEP")
                .is_ok_and(|v| matches!(v.as_str(), "0" | "false")),
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

    /// Use an explicitly registered persistent Claude config directory.
    #[must_use]
    pub fn with_claude_native_home(mut self, native_home: PathBuf) -> Self {
        self.claude_native_home = Some(native_home);
        self
    }

    /// Explicitly allow claude-print to use the host user's default login/config.
    #[must_use]
    pub fn with_inherited_default_claude_config(mut self) -> Self {
        self.claude_print_inherit_default_config = true;
        self
    }
}

/// Register `claude-print`, `claude-pty`, and `claude-bg` as per-instance factories.
pub fn native_driver_registry(config: NativeDriverConfig) -> Result<DriverRegistry, NodeError> {
    if config
        .claude_native_home
        .as_ref()
        .is_some_and(|path| !path.is_absolute())
    {
        return Err(NodeError::InvalidConfig(
            "REMUDA_CLAUDE_CONFIG_DIR must be an absolute path".into(),
        ));
    }
    if config.claude_print_inherit_default_config && config.claude_native_home.is_some() {
        return Err(NodeError::InvalidConfig(
            "REMUDA_CLAUDE_INHERIT_DEFAULT_CONFIG conflicts with REMUDA_CLAUDE_CONFIG_DIR".into(),
        ));
    }
    let registry = DriverRegistry::default();
    for kind in [
        DriverKind::ClaudePrint,
        DriverKind::ClaudePty,
        DriverKind::ClaudeBg,
        DriverKind::GenericPty,
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
        let overlay = if self.kind == DriverKind::GenericPty {
            None
        } else {
            launch
                .request
                .settings_overlay_path
                .as_deref()
                .map(str::trim)
                .filter(|path| !path.is_empty())
                .map(resolve_overlay_path)
                .transpose()?
        };
        let explicit_config_dir = launch
            .request
            .claude_config_dir
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(expand_host_path);
        if let Some(path) = explicit_config_dir.as_ref()
            && !path.is_absolute()
        {
            return Err(DriverError::Failed(
                "CLAUDE_CONFIG_DIR must be an absolute path".into(),
            ));
        }
        let delegation = parse_delegation(&launch.request);
        let inherit_default_config = self.kind != DriverKind::GenericPty
            && explicit_config_dir.is_none()
            && matches!(delegation, Delegation::None);
        let native_home = if let Some(path) = explicit_config_dir {
            path
        } else if inherit_default_config {
            default_claude_home()?
        } else {
            self.config
                .claude_native_home
                .clone()
                .unwrap_or_else(|| instance_dir.join("native-home"))
        };
        std::fs::create_dir_all(&launch_dir)
            .map_err(|error| DriverError::Failed(error.to_string()))?;
        if !inherit_default_config {
            std::fs::create_dir_all(&native_home)
                .map_err(|error| DriverError::Failed(error.to_string()))?;
        }
        let profile = provider_profile(&launch, delegation)?;
        let spec = instance_spec(&launch, &self.config, &profile)?;
        let binary = match self.kind {
            DriverKind::GenericPty => {
                let name = preset_by_id(match launch.request.kind {
                    remuda_protocol::AgentKind::Codex => "codex",
                    remuda_protocol::AgentKind::Grok => "grok",
                    remuda_protocol::AgentKind::Agy => "agy",
                    remuda_protocol::AgentKind::Generic => "gemini",
                    remuda_protocol::AgentKind::Claude => "claude",
                })
                .map(|preset| preset.binary)
                .unwrap_or("claude");
                if name == "claude" {
                    self.config
                        .claude_binary
                        .clone()
                        .map(BinarySource::Path)
                        .unwrap_or_else(|| BinarySource::Command(name.to_owned()))
                } else {
                    BinarySource::Command(name.to_owned())
                }
            }
            _ => self
                .config
                .claude_binary
                .clone()
                .map(BinarySource::Path)
                .unwrap_or_else(|| BinarySource::Command("claude".to_owned())),
        };
        let native: Arc<dyn NativeDriver> = match self.kind {
            DriverKind::ClaudePrint => {
                let mut options = ClaudePrintOptions::new(profile, launch_dir, native_home, binary);
                options.extra_env = self.config.extra_env.clone();
                options.handshake_timeout = self.config.print_handshake_timeout;
                options.inherit_default_config = inherit_default_config;
                options.settings_overlay_path = overlay.clone();
                Arc::new(ClaudePrintDriver::new(options))
            }
            DriverKind::ClaudePty => {
                let mut options = ClaudePtyOptions::new(profile, launch_dir, native_home, binary);
                options.extra_env = self.config.extra_env.clone();
                options.session_name = self.config.herdr_session.clone();
                options.socket_dir = self.config.herdr_socket_dir.clone();
                options.herdr_binary = self.config.herdr_binary.clone();
                options.inherit_default_config = inherit_default_config;
                options.settings_overlay_path = overlay.clone();
                Arc::new(ClaudePtyDriver::new(options))
            }
            DriverKind::ClaudeBg => {
                let mut options = ClaudeBgOptions::new(profile, launch_dir, native_home, binary);
                options.extra_env = self.config.extra_env.clone();
                options.session_name = self.config.herdr_session.clone();
                options.socket_dir = self.config.herdr_socket_dir.clone();
                options.herdr_binary = self.config.herdr_binary.clone();
                options.inherit_default_config = inherit_default_config;
                options.settings_overlay_path = overlay;
                Arc::new(ClaudeBgDriver::new(options))
            }
            DriverKind::GenericPty => {
                let mut options = GenericPtyOptions::new(profile, launch_dir, native_home, binary);
                options.extra_env = self.config.extra_env.clone();
                options.session_name = self.config.herdr_session.clone();
                options.socket_dir = self.config.herdr_socket_dir.clone();
                options.herdr_binary = self.config.herdr_binary.clone();
                Arc::new(GenericPtyDriver::new(options))
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
            recipe: std::sync::Mutex::new(None),
            startup_error: std::sync::Mutex::new(None),
        }))
    }
}

fn default_claude_home() -> Result<PathBuf, DriverError> {
    let home = std::env::var_os("HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| DriverError::Failed("HOME is required for default Claude config".into()))?;
    if !home.is_absolute() {
        return Err(DriverError::Failed(
            "HOME must be absolute for default Claude config".into(),
        ));
    }
    Ok(home.join(".claude"))
}

struct NativeAdapter {
    kind: DriverKind,
    native: Arc<dyn NativeDriver>,
    spec: InstanceSpec,
    recipe: std::sync::Mutex<Option<remuda_driver::LaunchRecipe>>,
    startup_error: std::sync::Mutex<Option<String>>,
}

impl Driver for NativeAdapter {
    fn track_pty_resources(
        &self,
        id: remuda_protocol::InstanceId,
        store: Arc<dyn remuda_driver::PtyResourceStore>,
    ) {
        self.native.track_pty_resources(id, store);
    }

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
            if let Ok(mut slot) = self.recipe.lock() {
                *slot = Some(handle.recipe().clone());
            }
            if let Ok(mut slot) = self.startup_error.lock() {
                *slot = handle.ack().native_ids.get("lastError").cloned();
            }
            Ok(Some(handle.into_events()))
        })
    }

    fn launch_recipe(&self) -> Option<remuda_driver::LaunchRecipe> {
        self.recipe.lock().ok().and_then(|slot| slot.clone())
    }

    fn startup_error(&self) -> Option<String> {
        self.startup_error.lock().ok().and_then(|slot| slot.clone())
    }

    fn wait_control(&self) -> DriverFuture<'_> {
        Box::pin(async move {
            NativeDriver::wait_control(&*self.native)
                .await
                .map_err(map_driver_error)?;
            Ok(Vec::new())
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
                DriverRequest::SendKeys { keys } => {
                    self.native
                        .send_keys(keys)
                        .await
                        .map_err(map_driver_error)?;
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

fn provider_profile(
    launch: &DriverLaunch,
    delegation: Delegation,
) -> Result<ProviderProfile, DriverError> {
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
        delegation,
        secret_ref: None,
        models: vec![model],
        health: ProviderHealth::Healthy,
    })
}

fn parse_delegation(request: &crate::CreateInstanceRequest) -> Delegation {
    let raw = request
        .delegation
        .as_deref()
        .filter(|value| !value.is_empty())
        .unwrap_or(request.provider_profile_id.as_str());
    match raw.trim().to_ascii_lowercase().as_str() {
        "gateway" => Delegation::Gateway,
        "direct" => Delegation::Direct,
        _ => Delegation::None,
    }
}

fn expand_host_path(raw: &str) -> PathBuf {
    let trimmed = raw.trim();
    if trimmed == "~" {
        return std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(trimmed));
    }
    if let Some(rest) = trimmed.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(trimmed)
}

fn resolve_overlay_path(raw: &str) -> Result<PathBuf, DriverError> {
    let path = expand_host_path(raw);
    if !path.is_absolute() {
        return Err(DriverError::Failed(
            "settings overlay path must be absolute after expanding ~".into(),
        ));
    }
    match path.metadata() {
        Ok(meta) if meta.is_file() => Ok(path),
        Ok(_) => Err(DriverError::Failed(
            "settings overlay path is not a file".into(),
        )),
        Err(_) => Err(DriverError::Failed(
            "settings overlay path does not exist".into(),
        )),
    }
}

fn with_max_budget(mut args: Vec<String>, budget: Option<&str>) -> Vec<String> {
    let Some(budget) = budget.map(str::trim).filter(|value| !value.is_empty()) else {
        return args;
    };
    if args.windows(2).any(|pair| pair[0] == "--max-budget-usd")
        || args
            .iter()
            .any(|token| token.starts_with("--max-budget-usd="))
    {
        return args;
    }
    args.push("--max-budget-usd".into());
    args.push(budget.to_owned());
    args
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
    let interaction = if matches!(
        launch.request.driver,
        DriverKind::ClaudePty | DriverKind::GenericPty
    ) {
        ClaudeInteractionMode::NativeTty
    } else {
        ClaudeInteractionMode::Host
    };
    let carrier = match launch.request.driver {
        DriverKind::ClaudePty | DriverKind::GenericPty => CarrierSpec::Pty(Box::new(PtyCarrier {
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
        args: with_max_budget(
            launch.request.args.clone(),
            launch.request.max_budget_usd.as_deref(),
        ),
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
        for (agent, kind) in [
            (AgentKind::Claude, DriverKind::ClaudePrint),
            (AgentKind::Claude, DriverKind::ClaudePty),
            (AgentKind::Claude, DriverKind::ClaudeBg),
            (AgentKind::Codex, DriverKind::GenericPty),
        ] {
            let instance =
                fixture_instance(InstanceId::new(), HostId::new(), WorkspaceId::new(), kind)
                    .expect("instance");
            let request = crate::CreateInstanceRequest {
                command_id: None,
                instance_id: Some(instance.meta.id.clone()),
                host_id: Some(instance.host_id.clone()),
                workspace_id: Some(instance.workspace_id.clone()),
                kind: agent,
                driver: kind,
                model: "haiku".to_owned(),
                args: vec!["--max-budget-usd".to_owned(), "0.3".to_owned()],
                provider_profile_id: "native".to_owned(),
                permission_mode: "manual".to_owned(),
                prompt: String::new(),
                cwd: None,
                delegation: None,
                settings_overlay_path: None,
                claude_config_dir: None,
                max_budget_usd: None,
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

    #[test]
    fn registered_claude_home_must_be_absolute() {
        let config = NativeDriverConfig::new(PathBuf::from("/tmp/remuda-node-test"))
            .with_claude_native_home(PathBuf::from("relative-home"));
        let error = match native_driver_registry(config) {
            Ok(_) => panic!("relative native home must fail"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("must be an absolute path"));
    }

    #[test]
    fn inherited_and_explicit_claude_homes_conflict() {
        let config = NativeDriverConfig::new(PathBuf::from("/tmp/remuda-node-test"))
            .with_claude_native_home(PathBuf::from("/tmp/claude-home"))
            .with_inherited_default_claude_config();
        let error = match native_driver_registry(config) {
            Ok(_) => panic!("conflicting native home modes must fail"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("conflicts"));
    }

    #[test]
    fn parse_delegation_prefers_explicit_field_then_profile_id() {
        let mut request = crate::CreateInstanceRequest {
            command_id: None,
            instance_id: None,
            host_id: None,
            workspace_id: None,
            kind: AgentKind::Claude,
            driver: DriverKind::ClaudePrint,
            model: "haiku".into(),
            args: Vec::new(),
            provider_profile_id: "none".into(),
            permission_mode: "dontAsk".into(),
            prompt: String::new(),
            cwd: None,
            delegation: Some("gateway".into()),
            settings_overlay_path: None,
            claude_config_dir: None,
            max_budget_usd: None,
        };
        assert_eq!(parse_delegation(&request), Delegation::Gateway);
        request.delegation = None;
        request.provider_profile_id = "gateway".into();
        assert_eq!(parse_delegation(&request), Delegation::Gateway);
        request.provider_profile_id = "native-login".into();
        assert_eq!(parse_delegation(&request), Delegation::None);
    }

    #[test]
    fn overlay_tilde_expands_and_missing_file_fails_closed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let overlay = dir.path().join("settings.relay.json");
        std::fs::write(&overlay, r#"{"model":"example"}"#).expect("overlay");
        let resolved = resolve_overlay_path(overlay.to_str().expect("utf8")).expect("exists");
        assert_eq!(resolved, overlay);
        let expanded = expand_host_path("~/settings.relay.json");
        assert!(expanded.is_absolute());
        assert!(expanded.ends_with("settings.relay.json"));
        let missing = dir.path().join("missing-overlay.json");
        let error = resolve_overlay_path(missing.to_str().expect("utf8")).expect_err("missing");
        assert!(error.to_string().contains("does not exist"), "{error}");
        assert!(
            !error.to_string().contains("ANTHROPIC"),
            "errors must not include overlay contents: {error}"
        );
    }
}
