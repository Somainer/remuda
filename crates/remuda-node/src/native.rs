//! Instance-local adapters for the native `remuda-driver` implementations.

use crate::{
    Driver, DriverError, DriverFactory, DriverFuture, DriverLaunch, DriverRegistry, DriverRequest,
    DriverStartFuture, NodeError,
};
use remuda_driver::claude_print::{ClaudePrintDriver, ClaudePrintOptions};
use remuda_driver::{
    BinarySource, ClaudeBgDriver, ClaudeBgOptions, ClaudeProviderOverlay, ClaudePtyDriver,
    ClaudePtyOptions, Delegation, Driver as NativeDriver, GenericPtyDriver, GenericPtyOptions,
    ProviderHealth, ProviderKind, ProviderProfile, Secret, ShellPtyDriver, ShellPtyOptions,
    preset_by_id, write_claude_provider_overlay,
};
use remuda_protocol::{
    ArgvInputPolicy, BgInputDelivery, CarrierSpec, ClaudeInteractionMode, ClaudePermission,
    ClaudePermissionMode, CompletionScope, ContentBlock, DriverInput, DriverKind,
    HerdrRepresentation, HerdrServer, HostId, Id, InputOrigin, InstanceSpec, InteractionAnswer,
    NativeHome, NativeHomeMode, PermissionMode, ProfileRef, PromptInput, PromptMode, PtyBackend,
    PtyCarrier, SchemaVersion, SettingsFormat, SettingsOverlay, TextBlock, U64,
};
use sha2::{Digest as _, Sha256};
use std::future::Future;
use std::pin::Pin;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

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
    /// Herdr socket directory; defaults to `data_dir/herdr` for PTY/background support.
    pub herdr_socket_dir: Option<PathBuf>,
    /// Explicit Herdr executable for PTY operations.
    pub herdr_binary: Option<PathBuf>,
    /// Isolated Herdr session, derived from `data_dir` unless explicitly configured.
    pub herdr_session: String,
    /// Close unknown panes on startup; disable only for deliberate manual recovery.
    pub herdr_orphan_sweep: bool,
    /// Automatically accept the exact Claude folder-trust dialog for registered workspaces.
    pub auto_trust_registered_workspaces: bool,
    /// Promote a `terminal` instance when a known agent CLI takes the PTY
    /// foreground, and hydrate its transcript (D-025).
    pub promote_terminal_agents: bool,
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
        let herdr_session =
            node_herdr_session(&data_dir, std::env::var("REMUDA_HERDR_SESSION").ok());
        let herdr_socket_dir = Some(data_dir.join("herdr"));
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
            herdr_socket_dir,
            herdr_binary: std::env::var_os("REMUDA_HERDR_BIN")
                .filter(|path| !path.is_empty())
                .map(PathBuf::from),
            herdr_session,
            herdr_orphan_sweep: !std::env::var("REMUDA_HERDR_ORPHAN_SWEEP")
                .is_ok_and(|v| matches!(v.as_str(), "0" | "false")),
            auto_trust_registered_workspaces: true,
            promote_terminal_agents: true,
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

fn node_herdr_session(data_dir: &Path, configured: Option<String>) -> String {
    configured.unwrap_or_else(|| {
        let digest = Sha256::digest(data_dir.as_os_str().as_encoded_bytes());
        // Keep named-session fallback sockets short, stable across restarts, and
        // distinct from the legacy shared `remuda-node` session.
        let suffix: String = digest[..12]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        format!("remuda-node-{suffix}")
    })
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
        DriverKind::ShellPty,
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
        std::fs::create_dir_all(&launch_dir)
            .map_err(|error| DriverError::Failed(error.to_string()))?;
        let delegation = parse_delegation(&launch.request);
        let profile = provider_profile(&launch, delegation)?;
        let overlay = resolve_claude_overlay(
            &launch.request,
            &launch_dir,
            self.kind,
            delegation,
            &profile,
        )?;
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
        #[cfg(target_os = "macos")]
        if matches!(
            self.kind,
            DriverKind::ClaudePrint | DriverKind::ClaudePty | DriverKind::ClaudeBg
        ) || (self.kind == DriverKind::GenericPty
            && launch.request.kind == remuda_protocol::AgentKind::Claude)
        {
            let inherited_home = (inherit_default_config && self.kind == DriverKind::ClaudePty)
                .then(|| std::env::var_os("CLAUDE_CONFIG_DIR"))
                .flatten()
                .map(PathBuf::from)
                .map(|path| launch.workspace_root.join(path));
            crate::native_config_access::check_claude_config_access(
                inherited_home.as_deref().unwrap_or(&native_home),
            )?;
        }
        if !inherit_default_config {
            crate::prepare_workspace(&native_home)
                .map_err(|error| DriverError::Failed(error.to_string()))?;
        }
        let spec = instance_spec(&launch, &self.config, &profile)?;
        let binary = match self.kind {
            DriverKind::GenericPty => {
                let name = preset_by_id(match launch.request.kind {
                    remuda_protocol::AgentKind::Codex => "codex",
                    remuda_protocol::AgentKind::Grok => "grok",
                    remuda_protocol::AgentKind::Agy => "agy",
                    remuda_protocol::AgentKind::Generic => "gemini",
                    remuda_protocol::AgentKind::Claude => "claude",
                    remuda_protocol::AgentKind::Terminal => "sh",
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
                options.extra_env = crate::origin::instance_env(&self.config.extra_env);
                options.agent_mcp = Some(crate::origin::instance_mcp(&launch));
                options.origin = launch.request.origin;
                options.handshake_timeout = self.config.print_handshake_timeout;
                options.inherit_default_config = inherit_default_config;
                options.settings_overlay_path = overlay.clone();
                Arc::new(ClaudePrintDriver::new(options))
            }
            DriverKind::ClaudePty => {
                let mut options = ClaudePtyOptions::new(profile, launch_dir, native_home, binary);
                options.extra_env = crate::origin::instance_env(&self.config.extra_env);
                options.agent_mcp = Some(crate::origin::instance_mcp(&launch));
                options.origin = launch.request.origin.into();
                options.session_name = self.config.herdr_session.clone();
                options.socket_dir = self.config.herdr_socket_dir.clone();
                options.herdr_binary = self.config.herdr_binary.clone();
                options.inherit_default_config = inherit_default_config;
                options.settings_overlay_path = overlay.clone();
                options.auto_trust_registered_workspace = self
                    .config
                    .auto_trust_registered_workspaces
                    && cwd_is_registered(&launch.workspace_root, &launch.registered_workspace_root);
                Arc::new(ClaudePtyDriver::new(options))
            }
            DriverKind::ClaudeBg => {
                let mut options = ClaudeBgOptions::new(profile, launch_dir, native_home, binary);
                options.extra_env = crate::origin::instance_env(&self.config.extra_env);
                options.agent_mcp = Some(crate::origin::instance_mcp(&launch));
                options.origin = launch.request.origin.into();
                options.session_name = self.config.herdr_session.clone();
                options.socket_dir = self.config.herdr_socket_dir.clone();
                options.herdr_binary = self.config.herdr_binary.clone();
                options.inherit_default_config = inherit_default_config;
                options.settings_overlay_path = overlay;
                Arc::new(ClaudeBgDriver::new(options))
            }
            DriverKind::GenericPty => {
                let mut options = GenericPtyOptions::new(profile, launch_dir, native_home, binary);
                options.extra_env = crate::origin::instance_env(&self.config.extra_env);
                options.agent_mcp = Some(crate::origin::instance_mcp(&launch));
                options.origin = launch.request.origin.into();
                options.session_name = self.config.herdr_session.clone();
                options.socket_dir = self.config.herdr_socket_dir.clone();
                options.herdr_binary = self.config.herdr_binary.clone();
                Arc::new(GenericPtyDriver::new(options))
            }
            DriverKind::ShellPty => {
                let mut options = ShellPtyOptions::login(launch.workspace_root.clone());
                options.extra_env = crate::origin::instance_env(&self.config.extra_env);
                options.agent_mcp = Some(crate::origin::instance_mcp(&launch));
                options.args = launch.request.args.clone();
                // D-025: watch for an agent CLI taking the foreground so the
                // structured view and composer follow what the human started.
                options.promote = self.config.promote_terminal_agents;
                options.claude_home = self.config.claude_native_home.clone();
                Arc::new(ShellPtyDriver::new(options))
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

fn cwd_is_registered(cwd: &Path, registered_root: &Path) -> bool {
    match (
        std::fs::canonicalize(cwd),
        std::fs::canonicalize(registered_root),
    ) {
        (Ok(cwd), Ok(root)) => cwd.starts_with(root),
        _ => false,
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

    fn tty_bridge(
        &self,
    ) -> Pin<Box<dyn Future<Output = Option<remuda_driver::TtyBridge>> + Send + '_>> {
        Box::pin(async move { self.native.tty_bridge().await })
    }

    fn execute(&self, request: DriverRequest) -> DriverFuture<'_> {
        Box::pin(async move {
            match request {
                DriverRequest::Send {
                    prompt,
                    attachments,
                    origin,
                } => {
                    self.native
                        .send(prompt_input(prompt, &attachments, origin))
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
                DriverRequest::Configure {
                    model,
                    effort,
                    effort_index,
                } => {
                    let switch = remuda_protocol::ModelSwitchInput {
                        model_id: model.clone().unwrap_or_default(),
                        effective: remuda_protocol::ModelEffective::NextTurn,
                        effort: effort.clone(),
                    };
                    let applied = format!(
                        "model={} effort={} index={}",
                        model.as_deref().unwrap_or("-"),
                        effort.as_deref().unwrap_or("-"),
                        effort_index
                            .map(|n| n.to_string())
                            .unwrap_or_else(|| "-".into())
                    );
                    match self
                        .native
                        .send(remuda_protocol::DriverInput::ModelSwitch(Box::new(switch)))
                        .await
                    {
                        Ok(_) => {
                            return Ok(vec![crate::driver::DriverEmission::NativeLifecycle {
                                name: "instance.configure".into(),
                                status: format!("applied {applied}"),
                                severity: remuda_protocol::Severity::Info,
                            }]);
                        }
                        Err(error) => {
                            return Ok(vec![crate::driver::DriverEmission::NativeLifecycle {
                                name: "instance.configure".into(),
                                status: format!("accepted-noop: {error}"),
                                severity: remuda_protocol::Severity::Info,
                            }]);
                        }
                    }
                }
            }
            Ok(Vec::new())
        })
    }
}

/// Build the driver-facing prompt.
///
/// Each materialized attachment contributes an `image` [`MediaBlock`] naming
/// the Hub object, immediately followed by a `resource` block carrying the
/// local `file://` path. The split is deliberate: `MediaBlock` has no path
/// field, and a driver that can inline bytes (claude-print) needs the path to
/// read them while a driver that can only mention a path (the PTY family)
/// needs the same path as text. The text block stays last so the prompt reads
/// naturally after the attachments.
fn prompt_input(
    prompt: String,
    attachments: &[crate::attachments::MaterializedAttachment],
    origin: InputOrigin,
) -> DriverInput {
    let mut blocks = Vec::with_capacity(attachments.len() * 2 + 1);
    for attachment in attachments {
        let object_id = remuda_protocol::Id::try_from(attachment.object_id.clone());
        let Ok(object_id) = object_id else {
            // An id the protocol will not brand cannot be referenced; the
            // resource block below still carries the usable local path.
            tracing::warn!(object_id = %attachment.object_id, "attachment id is not a protocol Id");
            continue;
        };
        blocks.push(ContentBlock::Image(Box::new(remuda_protocol::MediaBlock {
            object_id: object_id.clone(),
            media_type: attachment.media_type.clone(),
            name: attachment
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned),
        })));
        blocks.push(ContentBlock::Resource(Box::new(
            remuda_protocol::ResourceBlock {
                uri: format!("file://{}", attachment.path.display()),
                media_type: remuda_protocol::Knowledge::Known {
                    value: attachment.media_type.clone(),
                },
                object_id: Some(object_id),
            },
        )));
    }
    blocks.push(ContentBlock::Text(Box::new(TextBlock { text: prompt })));
    DriverInput::Prompt(Box::new(PromptInput {
        mode: PromptMode::NewTurn,
        blocks,
        origin,
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

fn resolve_claude_overlay(
    request: &crate::CreateInstanceRequest,
    launch_dir: &Path,
    kind: DriverKind,
    delegation: Delegation,
    profile: &ProviderProfile,
) -> Result<Option<PathBuf>, DriverError> {
    if matches!(kind, DriverKind::GenericPty | DriverKind::ShellPty) {
        return Ok(None);
    }
    if let Some(overlay) = request.provider_overlay.as_ref() {
        refuse_host_scoped_overlay(overlay, request.host_id.as_ref())?;
    }
    let user = request
        .settings_overlay_path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(resolve_overlay_path)
        .transpose()?;
    if delegation != Delegation::Gateway {
        return Ok(user);
    }
    if let Some(path) = try_write_delivered_overlay(request, launch_dir)? {
        return Ok(Some(path));
    }
    match try_write_generated_gateway_overlay(profile, launch_dir, &request.model) {
        Ok(path) => Ok(Some(path)),
        Err(_) => user
            .ok_or_else(|| {
                DriverError::Failed("gateway delegation requires a settings overlay".into())
            })
            .map(Some),
    }
}

fn refuse_host_scoped_overlay(
    overlay: &serde_json::Value,
    host_id: Option<&HostId>,
) -> Result<(), DriverError> {
    let Some(scope) = overlay.get("scope").and_then(serde_json::Value::as_str) else {
        return Ok(());
    };
    let Some(wanted) = scope.strip_prefix("host:") else {
        return Ok(());
    };
    let got = host_id.map(|id| id.as_id().as_str()).unwrap_or("");
    if got != wanted {
        return Err(DriverError::Failed(format!(
            "host-scoped provider {scope} cannot be used on host {got}"
        )));
    }
    Ok(())
}

fn try_write_delivered_overlay(
    request: &crate::CreateInstanceRequest,
    launch_dir: &Path,
) -> Result<Option<PathBuf>, DriverError> {
    let Some(overlay) = request.provider_overlay.as_ref() else {
        return Ok(None);
    };
    let Some(token) = request
        .provider_auth_token
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Ok(None);
    };
    let base_url = overlay
        .get("baseUrl")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let model = overlay
        .get("model")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(request.model.as_str());
    let extra = overlay
        .get("headers")
        .and_then(serde_json::Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(k, v)| v.as_str().map(|value| (k.clone(), value.to_string())))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    let secret = Secret::new(token.as_bytes().to_vec());
    let kind = overlay
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("gateway");
    let delegation = if kind == "direct" {
        Delegation::Direct
    } else {
        Delegation::Gateway
    };
    write_claude_provider_overlay(
        launch_dir,
        &ClaudeProviderOverlay {
            delegation,
            base_url,
            model,
            secret: &secret,
            extra_env: &extra,
        },
    )
    .map(Some)
    .map_err(map_driver_error)
}

fn try_write_generated_gateway_overlay(
    profile: &ProviderProfile,
    launch_dir: &Path,
    model: &str,
) -> Result<PathBuf, remuda_driver::DriverError> {
    if profile.base_url.trim().is_empty() {
        return Err(remuda_driver::DriverError::SettingsIsolationUnavailable(
            "gateway profile has no base_url".into(),
        ));
    }
    let secret_ref = profile.secret_ref.as_ref().ok_or_else(|| {
        remuda_driver::DriverError::SettingsIsolationUnavailable(
            "gateway profile has no secret_ref".into(),
        )
    })?;
    let name = secret_ref.env_name().ok_or_else(|| {
        remuda_driver::DriverError::SettingsIsolationUnavailable(
            "generated overlay at factory build requires an env secret_ref".into(),
        )
    })?;
    let value = std::env::var(name).map_err(|_| {
        remuda_driver::DriverError::CredentialUnavailable(format!(
            "environment variable {name} is unset"
        ))
    })?;
    let secret = Secret::new(value.into_bytes());
    let extra = BTreeMap::new();
    write_claude_provider_overlay(
        launch_dir,
        &ClaudeProviderOverlay {
            delegation: Delegation::Gateway,
            base_url: &profile.base_url,
            model,
            secret: &secret,
            extra_env: &extra,
        },
    )
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
        DriverKind::ClaudePty | DriverKind::GenericPty | DriverKind::ShellPty
    ) {
        ClaudeInteractionMode::NativeTty
    } else {
        ClaudeInteractionMode::Host
    };
    let carrier = match launch.request.driver {
        DriverKind::ShellPty => CarrierSpec::ShellPty,
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
    match error {
        remuda_driver::DriverError::ControlUnavailable => DriverError::ControlUnavailable,
        other => DriverError::Failed(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::fixture_instance;
    use remuda_protocol::{AgentKind, HostId, InstanceId, WorkspaceId};

    #[test]
    fn herdr_defaults_isolate_data_roots_and_preserve_explicit_session() {
        let first_dir = PathBuf::from("/tmp/remuda-isolation/first");
        let second_dir = PathBuf::from("/tmp/remuda-isolation/second");
        let first = NativeDriverConfig::new(first_dir.clone());
        let second = NativeDriverConfig::new(second_dir.clone());
        assert_eq!(first.herdr_socket_dir, Some(first_dir.join("herdr")));
        assert_eq!(second.herdr_socket_dir, Some(second_dir.join("herdr")));
        let first_name = node_herdr_session(&first_dir, None);
        let second_name = node_herdr_session(&second_dir, None);
        assert_eq!(first_name, node_herdr_session(&first_dir, None));
        assert_ne!(first_name, second_name);
        assert_ne!(first_name, "remuda-node");
        assert_ne!(first_name, "default");
        assert_ne!(
            remuda_herdr::session_sockets(&first_name).api,
            remuda_herdr::session_sockets("remuda-node").api,
        );
        assert_ne!(
            remuda_herdr::session_sockets(&first_name).api,
            remuda_herdr::session_sockets(&second_name).api,
        );
        assert_eq!(
            node_herdr_session(&first_dir, Some("operator-session".into())),
            "operator-session",
        );
    }

    #[tokio::test]
    async fn orphan_sweep_leaves_another_nodes_workspace_untouched() {
        use remuda_herdr::{Client, WorkspaceCreateParams};
        use remuda_testing::{FakeHerdrOptions, FakeHerdrServer};

        let root = tempfile::tempdir().unwrap();
        let mut servers = Vec::new();
        let mut clients = Vec::new();
        for name in ["first", "second"] {
            let config = NativeDriverConfig::new(root.path().join(name));
            let socket_dir = config.herdr_socket_dir.unwrap();
            std::fs::create_dir_all(&socket_dir).unwrap();
            let socket = socket_dir.join("herdr.sock");
            servers.push(FakeHerdrServer::spawn(FakeHerdrOptions::new(&socket)).unwrap());
            let client = Client::connect(socket);
            client
                .workspace_create(WorkspaceCreateParams {
                    label: Some(format!("{name}-workspace")),
                    ..Default::default()
                })
                .await
                .unwrap();
            clients.push(client);
        }
        let other_before = clients[1].session_snapshot().await.unwrap();
        let first_dir = root.path().join("first");
        let node = crate::compose(&crate::ServeConfig::native(
            crate::DevServerConfig::loopback(0)
                .with_workspace_root(first_dir.clone())
                .with_workspace_roots(vec![root.path().to_path_buf()]),
            first_dir,
        ))
        .unwrap();
        node.reconcile_herdr().await.unwrap();
        assert!(
            clients[0]
                .session_snapshot()
                .await
                .unwrap()
                .workspaces
                .is_empty()
        );
        let other_after = clients[1].session_snapshot().await.unwrap();
        assert_eq!(other_after.workspaces.len(), 1);
        assert_eq!(
            other_after.workspaces[0].workspace_id,
            other_before.workspaces[0].workspace_id,
        );
        assert_eq!(other_after.panes.len(), other_before.panes.len());
        node.shutdown().await.unwrap();
    }

    #[test]
    fn automatic_trust_requires_canonical_registered_containment() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let child = root.join("subdirectory");
        let sibling = dir.path().join("repo-other");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        assert!(NativeDriverConfig::new(dir.path().into()).auto_trust_registered_workspaces);
        assert!(cwd_is_registered(&root, &root));
        assert!(cwd_is_registered(&child, &root));
        assert!(!cwd_is_registered(&sibling, &root));
        assert!(!cwd_is_registered(&root.join("missing"), &root));
        #[cfg(unix)]
        {
            let escape = root.join("escape");
            std::os::unix::fs::symlink(&sibling, &escape).unwrap();
            assert!(!cwd_is_registered(&escape, &root));
        }
    }

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
                origin: InputOrigin::Human,
                agent_credential: None,
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
                provider_overlay: None,
                provider_auth_token: None,
            };
            let driver = registry
                .build(
                    kind,
                    DriverLaunch {
                        instance,
                        request,
                        workspace_root: dir.path().to_path_buf(),
                        registered_workspace_root: dir.path().to_path_buf(),
                    },
                )
                .expect("build driver");
            assert_eq!(driver.kind(), kind);
        }
    }

    #[test]
    fn native_prompt_preserves_each_submitting_origin() {
        for origin in [InputOrigin::Human, InputOrigin::Bot, InputOrigin::Agent] {
            let DriverInput::Prompt(prompt) = prompt_input("new input".into(), &[], origin) else {
                panic!("prompt expected")
            };
            assert_eq!(prompt.origin, origin);
            assert_eq!(prompt.blocks.len(), 1, "a text-only send carries one block");
        }
    }

    /// D-027: each attachment contributes an image block naming the Hub object
    /// plus a resource block carrying the local path, and the text stays last.
    #[test]
    fn attachments_become_image_and_resource_blocks_before_the_text() {
        let attachment = crate::attachments::MaterializedAttachment {
            object_id: remuda_protocol::Id::new("obj").expect("id").to_string(),
            media_type: "image/png".into(),
            path: PathBuf::from("/tmp/remuda-node-test/attachments/shot.png"),
            byte_len: 64,
        };
        let DriverInput::Prompt(prompt) = prompt_input(
            "what colour?".into(),
            std::slice::from_ref(&attachment),
            InputOrigin::Human,
        ) else {
            panic!("prompt expected")
        };
        assert_eq!(prompt.blocks.len(), 3);
        match &prompt.blocks[0] {
            ContentBlock::Image(media) => {
                assert_eq!(media.object_id.to_string(), attachment.object_id);
                assert_eq!(media.media_type, "image/png");
                assert_eq!(media.name.as_deref(), Some("shot.png"));
            }
            other => panic!("expected an image block, got {other:?}"),
        }
        match &prompt.blocks[1] {
            ContentBlock::Resource(resource) => {
                assert_eq!(
                    resource.uri, "file:///tmp/remuda-node-test/attachments/shot.png",
                    "a driver that can only mention a path needs it verbatim"
                );
                assert_eq!(
                    resource.object_id.as_ref().map(ToString::to_string),
                    Some(attachment.object_id.clone())
                );
            }
            other => panic!("expected a resource block, got {other:?}"),
        }
        match &prompt.blocks[2] {
            ContentBlock::Text(text) => assert_eq!(text.text, "what colour?"),
            other => panic!("expected the text last, got {other:?}"),
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
            origin: InputOrigin::Human,
            agent_credential: None,
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
            provider_overlay: None,
            provider_auth_token: None,
        };
        assert_eq!(parse_delegation(&request), Delegation::Gateway);
        request.delegation = None;
        request.provider_profile_id = "gateway".into();
        assert_eq!(parse_delegation(&request), Delegation::Gateway);
        request.provider_profile_id = "native-login".into();
        assert_eq!(parse_delegation(&request), Delegation::None);
    }

    #[test]
    fn gateway_falls_back_to_user_overlay_when_generated_overlay_is_unavailable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let overlay = dir.path().join("settings.overlay.json");
        std::fs::write(&overlay, r#"{"model":"example"}"#).expect("overlay");
        let registry = native_driver_registry(NativeDriverConfig::new(dir.path().to_path_buf()))
            .expect("registry");
        let instance = fixture_instance(
            InstanceId::new(),
            HostId::new(),
            WorkspaceId::new(),
            DriverKind::ClaudePrint,
        )
        .expect("instance");
        let request = crate::CreateInstanceRequest {
            origin: remuda_protocol::InputOrigin::Agent,
            agent_credential: None,
            command_id: None,
            instance_id: Some(instance.meta.id.clone()),
            host_id: Some(instance.host_id.clone()),
            workspace_id: Some(instance.workspace_id.clone()),
            kind: AgentKind::Claude,
            driver: DriverKind::ClaudePrint,
            model: "haiku".into(),
            args: Vec::new(),
            provider_profile_id: "gateway".into(),
            permission_mode: "dontAsk".into(),
            prompt: String::new(),
            cwd: None,
            delegation: Some("gateway".into()),
            settings_overlay_path: Some(overlay.to_string_lossy().into_owned()),
            claude_config_dir: None,
            max_budget_usd: None,
            provider_overlay: None,
            provider_auth_token: None,
        };
        registry
            .build(
                DriverKind::ClaudePrint,
                DriverLaunch {
                    instance,
                    request,
                    workspace_root: dir.path().to_path_buf(),
                    registered_workspace_root: dir.path().to_path_buf(),
                },
            )
            .expect("user overlay must be accepted while generated overlay is unavailable");
    }

    #[test]
    fn gateway_without_overlay_fails_closed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = native_driver_registry(NativeDriverConfig::new(dir.path().to_path_buf()))
            .expect("registry");
        let instance = fixture_instance(
            InstanceId::new(),
            HostId::new(),
            WorkspaceId::new(),
            DriverKind::ClaudePrint,
        )
        .expect("instance");
        let request = crate::CreateInstanceRequest {
            origin: remuda_protocol::InputOrigin::Agent,
            agent_credential: None,
            command_id: None,
            instance_id: Some(instance.meta.id.clone()),
            host_id: Some(instance.host_id.clone()),
            workspace_id: Some(instance.workspace_id.clone()),
            kind: AgentKind::Claude,
            driver: DriverKind::ClaudePrint,
            model: "haiku".into(),
            args: Vec::new(),
            provider_profile_id: "gateway".into(),
            permission_mode: "dontAsk".into(),
            prompt: String::new(),
            cwd: None,
            delegation: Some("gateway".into()),
            settings_overlay_path: None,
            claude_config_dir: None,
            max_budget_usd: None,
            provider_overlay: None,
            provider_auth_token: None,
        };
        let error = match registry.build(
            DriverKind::ClaudePrint,
            DriverLaunch {
                instance,
                request,
                workspace_root: dir.path().to_path_buf(),
                registered_workspace_root: dir.path().to_path_buf(),
            },
        ) {
            Ok(_) => panic!("gateway without overlay must fail closed"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("settings overlay"), "{error}");
    }

    #[test]
    fn host_scoped_overlay_is_refused_on_the_wrong_host() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = native_driver_registry(NativeDriverConfig::new(dir.path().to_path_buf()))
            .expect("registry");
        let instance = fixture_instance(
            InstanceId::new(),
            HostId::new(),
            WorkspaceId::new(),
            DriverKind::ClaudePrint,
        )
        .expect("instance");
        let request = crate::CreateInstanceRequest {
            origin: remuda_protocol::InputOrigin::Agent,
            agent_credential: None,
            command_id: None,
            instance_id: Some(instance.meta.id.clone()),
            host_id: Some(instance.host_id.clone()),
            workspace_id: Some(instance.workspace_id.clone()),
            kind: AgentKind::Claude,
            driver: DriverKind::ClaudePrint,
            model: "haiku".into(),
            args: Vec::new(),
            provider_profile_id: "pvp_other".into(),
            permission_mode: "dontAsk".into(),
            prompt: String::new(),
            cwd: None,
            delegation: Some("gateway".into()),
            settings_overlay_path: None,
            claude_config_dir: None,
            max_budget_usd: None,
            provider_overlay: Some(serde_json::json!({
                "profileId": "pvp_other",
                "kind": "gateway",
                "baseUrl": "http://127.0.0.1:1",
                "model": "haiku",
                "scope": "host:hst_other"
            })),
            provider_auth_token: Some("sk-fake-host-scoped".into()),
        };
        let error = match registry.build(
            DriverKind::ClaudePrint,
            DriverLaunch {
                instance,
                request,
                workspace_root: dir.path().to_path_buf(),
                registered_workspace_root: dir.path().to_path_buf(),
            },
        ) {
            Ok(_) => panic!("wrong-host scoped overlay must fail"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("host-scoped provider"),
            "{error}"
        );
    }

    #[test]
    fn overlay_tilde_expands_and_missing_file_fails_closed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let overlay = dir.path().join("settings.overlay.json");
        std::fs::write(&overlay, r#"{"model":"example"}"#).expect("overlay");
        let resolved = resolve_overlay_path(overlay.to_str().expect("utf8")).expect("exists");
        assert_eq!(resolved, overlay);
        let expanded = expand_host_path("~/settings.overlay.json");
        assert!(expanded.is_absolute());
        assert!(expanded.ends_with("settings.overlay.json"));
        let missing = dir.path().join("missing-overlay.json");
        let error = resolve_overlay_path(missing.to_str().expect("utf8")).expect_err("missing");
        assert!(error.to_string().contains("does not exist"), "{error}");
        assert!(
            !error.to_string().contains("ANTHROPIC"),
            "errors must not include overlay contents: {error}"
        );
    }
}
