//! Turn `InstanceSpec` + [`ProviderProfile`] into a serializable [`LaunchRecipe`].

use crate::binary::{BinaryPin, pin_binary};
use crate::error::{DriverError, DriverResult};
use crate::flags::{reject_banned_env, validate_spec_args};
use crate::profile::{Delegation, ProviderHealth, ProviderKind, ProviderProfile, SecretRef};
use crate::recipe::{
    EnvAllowlistEntry, EnvAllowlistSource, FileLifetime, FileRole, LaunchAudit, LaunchRecipe,
    MaterializedFile, RecipePermission, RecipeProvider, TECH_DEBT_M0_PERM_01,
};
use remuda_protocol::{
    AgentKind, ApprovalAuthority, BoolLiteral, ClaudePermissionMode, CommandOrigin, DriverKind,
    EnvBinding, Id, InputDelivery, InputOrigin, InstanceSpec, PermissionMode,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// How the native session identity is passed on argv.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionAction {
    /// New print/pty session with `--session-id`.
    New {
        /// UUID consumed by Claude `--session-id`.
        session_id: String,
    },
    /// Resume with `--resume <exact uuid>`. Never `--continue`.
    Resume {
        /// Exact native session UUID.
        session_id: String,
    },
}

impl SessionAction {
    /// Session UUID, when the driver uses one.
    pub fn session_id(&self) -> &str {
        match self {
            Self::New { session_id } | Self::Resume { session_id } => session_id,
        }
    }
}

/// Where the native binary comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BinarySource {
    /// Look up a command name on `PATH` and pin it.
    Command(String),
    /// Pin this absolute path.
    Path(PathBuf),
    /// Already pinned (tests / FakeDriver).
    Pinned(BinaryPin),
}

/// Inputs required to materialize a launch. Same inputs yield the same recipe.
#[derive(Debug, Clone)]
pub struct MaterializeRequest<'a> {
    /// Instance spec.
    pub spec: &'a InstanceSpec,
    /// Thin provider profile.
    pub profile: &'a ProviderProfile,
    /// Directory for 0700 launch files (settings overlay).
    pub launch_dir: PathBuf,
    /// Registered native home (`CLAUDE_CONFIG_DIR` / `CODEX_HOME` / …).
    pub native_home: PathBuf,
    /// New vs resume session identity.
    pub session: SessionAction,
    /// Launch identity persisted on the recipe.
    pub launch_id: Id,
    /// Binary to pin.
    pub binary: BinarySource,
    /// Override `--setting-sources`. Default `user,project,local`.
    pub setting_sources: Option<Vec<String>>,
    /// Who originated this launch. Bot/dispatcher specs cannot request bypass/yolo.
    pub origin: LaunchOrigin,
}

/// Node token-broker bind baked into a Claude `apiKeyHelper` script.
///
/// The token is a per-instance bearer for the UDS broker. It is written only
/// into the owner-only helper script, never into the recipe or settings JSON.
#[derive(Clone)]
pub struct TokenBrokerBind {
    /// Absolute path of the broker Unix socket (`0600`).
    pub socket_path: PathBuf,
    /// Instance id the broker allowlists.
    pub instance_id: String,
    /// Per-instance token presented by the helper. Never log this.
    pub token: String,
}

impl fmt::Debug for TokenBrokerBind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TokenBrokerBind")
            .field("socket_path", &self.socket_path)
            .field("instance_id", &self.instance_id)
            .field("token", &"[redacted]")
            .finish()
    }
}

/// Origin used to gate yolo/bypass. Dispatcher commands are [`CommandOrigin::Bot`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LaunchOrigin {
    /// Human UI/CLI (and other non-bot command origins).
    #[default]
    Human,
    /// Bot or dispatcher path. Cannot request bypass (D-011).
    Bot,
}

impl From<InputOrigin> for LaunchOrigin {
    fn from(value: InputOrigin) -> Self {
        match value {
            InputOrigin::Human => Self::Human,
            InputOrigin::Bot | InputOrigin::Agent => Self::Bot,
        }
    }
}

impl From<CommandOrigin> for LaunchOrigin {
    fn from(value: CommandOrigin) -> Self {
        match value {
            CommandOrigin::Bot => Self::Bot,
            CommandOrigin::Ui | CommandOrigin::Cli | CommandOrigin::Mcp | CommandOrigin::System => {
                Self::Human
            }
        }
    }
}

/// Materialize `spec` + `profile` into a durable recipe and 0600 overlay files.
///
/// Idempotent for identical [`MaterializeRequest`] values: overlay bytes, argv,
/// and pin are stable. Secret values and prompts are never written.
pub fn materialize(request: &MaterializeRequest<'_>) -> DriverResult<LaunchRecipe> {
    materialize_inner(request, None)
}

/// Like [`materialize`], and write a Claude `apiKeyHelper` bound to `broker`.
///
/// The helper script authenticates with `broker.token` and prints the secret
/// for `profile.secret_ref`. Settings overlay points `apiKeyHelper` at the
/// script and does not inject `ANTHROPIC_AUTH_TOKEN`.
pub fn materialize_with_token_broker(
    request: &MaterializeRequest<'_>,
    broker: &TokenBrokerBind,
) -> DriverResult<LaunchRecipe> {
    materialize_inner(request, Some(broker))
}

fn materialize_inner(
    request: &MaterializeRequest<'_>,
    broker: Option<&TokenBrokerBind>,
) -> DriverResult<LaunchRecipe> {
    validate_spec_profile(request.spec, request.profile)?;
    validate_paths(request)?;
    if request.profile.delegation == Delegation::Direct {
        return Err(DriverError::DirectDelegationV2);
    }
    let extras = validate_spec_args(request.spec.driver, &request.spec.args)?;
    reject_spec_env(request.spec)?;
    let setting_sources = setting_sources(request)?;
    let binary = pin_source(&request.binary)?;
    let model = resolve_model(request.spec, request.profile)?;
    let (permission, debt, approval) = permission_plan(request.spec, request.origin)?;
    let inject_provider = request.profile.delegation == Delegation::Gateway;

    fs::create_dir_all(&request.launch_dir)?;
    set_dir_mode(&request.launch_dir, 0o700)?;

    let mut files = Vec::new();
    let mut env_allowlist = Vec::new();
    let mut api_key_helper_path: Option<PathBuf> = None;
    let mut argv;
    let input_delivery;
    let session_id;

    if request.spec.driver == DriverKind::ShellPty {
        return Err(DriverError::InvalidLaunchSpec(
            "shell-pty does not use the Claude/generic materializer".into(),
        ));
    }

    match request.spec.driver {
        DriverKind::ClaudePrint | DriverKind::ClaudePty | DriverKind::ClaudeBg => {
            let settings_path = if inject_provider {
                if let Some(bind) = broker {
                    api_key_helper_path = maybe_write_api_key_helper(request, bind, &mut files)?;
                }
                let path = request.launch_dir.join("settings.json");
                let settings =
                    claude_settings_json(request.profile, &model, api_key_helper_path.as_deref())?;
                let bytes = serde_json::to_vec_pretty(&settings)?;
                write_private_file(&path, &bytes)?;
                let digest = crate::binary::hash_bytes(&bytes)?;
                files.push(MaterializedFile {
                    path: path.to_string_lossy().into_owned(),
                    role: FileRole::Settings,
                    mode: "0600".into(),
                    content_digest: digest,
                    lifetime: FileLifetime::Launch,
                });
                Some(path)
            } else {
                None
            };
            argv = claude_argv(
                request.spec.driver,
                &permission,
                &setting_sources,
                settings_path.as_deref(),
                &model,
                &request.session,
                &extras,
            )?;
            input_delivery = match request.spec.driver {
                DriverKind::ClaudePrint => InputDelivery::Stdio,
                DriverKind::ClaudePty => InputDelivery::Tty,
                DriverKind::ClaudeBg => InputDelivery::DeferredArgv,
                _ => InputDelivery::Stdio,
            };
            session_id = match request.spec.driver {
                DriverKind::ClaudeBg => None,
                _ => Some(request.session.session_id().to_string()),
            };
            push_env(
                &mut env_allowlist,
                "CLAUDE_CONFIG_DIR",
                EnvAllowlistSource::NativeHome,
                None,
            );
            if inject_provider {
                push_env(
                    &mut env_allowlist,
                    "ANTHROPIC_BASE_URL",
                    EnvAllowlistSource::ProviderOverlay,
                    None,
                );
                match request.profile.secret_ref.as_ref() {
                    Some(secret)
                        if secret.helper_command().is_some() || api_key_helper_path.is_some() => {}
                    Some(secret) => push_env(
                        &mut env_allowlist,
                        "ANTHROPIC_AUTH_TOKEN",
                        EnvAllowlistSource::Credential,
                        Some(secret.as_str().to_string()),
                    ),
                    None => {
                        return Err(DriverError::InvalidLaunchSpec(
                            "gateway delegation requires a secret_ref".into(),
                        ));
                    }
                }
            }
        }
        DriverKind::CodexAppserver => {
            argv = vec!["app-server".into(), "--listen".into(), "stdio://".into()];
            argv.extend(extras);
            input_delivery = InputDelivery::Stdio;
            session_id = None;
            push_env(
                &mut env_allowlist,
                "CODEX_HOME",
                EnvAllowlistSource::NativeHome,
                None,
            );
            if inject_provider {
                let secret = request.profile.secret_ref.as_ref().ok_or_else(|| {
                    DriverError::InvalidLaunchSpec(
                        "gateway delegation requires a secret_ref".into(),
                    )
                })?;
                push_env(
                    &mut env_allowlist,
                    "RUNTIME_PROVIDER_TOKEN",
                    EnvAllowlistSource::Credential,
                    Some(secret.as_str().to_string()),
                );
            }
        }
        DriverKind::GrokAcp => {
            argv = vec![
                "agent".into(),
                "--no-leader".into(),
                "--model".into(),
                model.clone(),
                "stdio".into(),
            ];
            argv.extend(extras);
            input_delivery = InputDelivery::Stdio;
            session_id = None;
            push_env(
                &mut env_allowlist,
                "GROK_HOME",
                EnvAllowlistSource::NativeHome,
                None,
            );
            push_env(
                &mut env_allowlist,
                "GROK_DISABLE_AUTOUPDATER",
                EnvAllowlistSource::ProviderOverlay,
                None,
            );
            if inject_provider {
                let secret = request.profile.secret_ref.as_ref().ok_or_else(|| {
                    DriverError::InvalidLaunchSpec(
                        "gateway delegation requires a secret_ref".into(),
                    )
                })?;
                push_env(
                    &mut env_allowlist,
                    "RUNTIME_PROVIDER_TOKEN",
                    EnvAllowlistSource::Credential,
                    Some(secret.as_str().to_string()),
                );
            }
        }
        DriverKind::AgyPrint => {
            argv = vec![
                "--output-format".into(),
                "stream-json".into(),
                "--model".into(),
                model.clone(),
            ];
            argv.extend(extras);
            input_delivery = InputDelivery::DeferredArgv;
            session_id = None;
            if inject_provider {
                let secret = request.profile.secret_ref.as_ref().ok_or_else(|| {
                    DriverError::InvalidLaunchSpec(
                        "gateway delegation requires a secret_ref".into(),
                    )
                })?;
                push_env(
                    &mut env_allowlist,
                    "GEMINI_API_KEY",
                    EnvAllowlistSource::Credential,
                    Some(secret.as_str().to_string()),
                );
            }
        }
        DriverKind::GenericPty => {
            argv = extras;
            input_delivery = InputDelivery::Tty;
            session_id = None;
        }
        DriverKind::ShellPty => {
            argv = extras;
            input_delivery = InputDelivery::Tty;
            session_id = None;
        }
    }

    collect_spec_env(request.spec, request.profile.delegation, &mut env_allowlist)?;

    let settings_digest = files
        .iter()
        .find(|file| file.role == FileRole::Settings)
        .map(|file| file.content_digest.clone());
    let redacted_argv = redact_argv(&argv, &files);
    let env_names = env_allowlist.iter().map(|e| e.name.clone()).collect();
    let mut credential_refs: Vec<String> = env_allowlist
        .iter()
        .filter_map(|e| e.secret_ref.clone())
        .collect();
    if api_key_helper_path.is_some()
        && let Some(secret) = request.profile.secret_ref.as_ref()
    {
        let spelling = secret.as_str().to_string();
        if !credential_refs.iter().any(|entry| entry == &spelling) {
            credential_refs.push(spelling);
        }
    }

    let recipe = LaunchRecipe {
        launch_id: request.launch_id.clone(),
        driver: request.spec.driver,
        binary,
        cwd: request.spec.cwd.clone(),
        argv,
        env_allowlist,
        materialized_files: files,
        setting_sources,
        session_id,
        native_home: request.native_home.to_string_lossy().into_owned(),
        input_delivery,
        provider: RecipeProvider {
            profile_id: request.profile.id.clone(),
            kind: request.profile.kind,
            base_url: request.profile.base_url.clone(),
            delegation: request.profile.delegation,
            secret_ref: request
                .profile
                .secret_ref
                .as_ref()
                .map(|secret| secret.as_str().to_string()),
            model_requested: model,
        },
        permission,
        technical_debt: debt,
        audit: LaunchAudit {
            env_names,
            credential_refs,
            redacted_argv,
            settings_digest,
            prohibited_options_checked: BoolLiteral,
            approval_authority: approval,
        },
    };
    tracing::info!(
        launch_id = %recipe.launch_id,
        driver = ?recipe.driver,
        binary = %recipe.binary.abs_path,
        debt = ?recipe.technical_debt,
        "materialized launch recipe"
    );
    Ok(recipe)
}

fn validate_spec_profile(spec: &InstanceSpec, profile: &ProviderProfile) -> DriverResult<()> {
    if spec.driver == DriverKind::GenericPty {
        if crate::generic_pty::preset_for_spec(spec).is_err() {
            return Err(DriverError::InvalidLaunchSpec(
                "generic-pty has no preset for this agent kind".into(),
            ));
        }
    } else if spec.kind != agent_kind(spec.driver) {
        return Err(DriverError::InvalidLaunchSpec(format!(
            "driver {:?} is incompatible with agent kind",
            spec.driver
        )));
    }
    if profile.health != ProviderHealth::Healthy {
        return Err(DriverError::ProviderUnavailable(format!(
            "profile health is {:?}",
            profile.health
        )));
    }
    if !provider_matches(spec.driver, profile.kind) {
        return Err(DriverError::ProviderProtocolMismatch(format!(
            "profile kind {:?} does not match driver {:?}",
            profile.kind, spec.driver
        )));
    }
    Ok(())
}

fn agent_kind(driver: DriverKind) -> AgentKind {
    match driver {
        DriverKind::ClaudePrint | DriverKind::ClaudePty | DriverKind::ClaudeBg => AgentKind::Claude,
        DriverKind::CodexAppserver => AgentKind::Codex,
        DriverKind::GrokAcp => AgentKind::Grok,
        DriverKind::AgyPrint => AgentKind::Agy,
        DriverKind::GenericPty => AgentKind::Generic,
        DriverKind::ShellPty => AgentKind::Terminal,
    }
}

fn provider_matches(driver: DriverKind, kind: ProviderKind) -> bool {
    match driver {
        DriverKind::ClaudePrint | DriverKind::ClaudePty | DriverKind::ClaudeBg => {
            kind == ProviderKind::Anthropic
        }
        DriverKind::CodexAppserver => kind == ProviderKind::OpenaiResponses,
        DriverKind::GrokAcp => kind == ProviderKind::Xai || kind == ProviderKind::OpenaiResponses,
        DriverKind::AgyPrint => kind == ProviderKind::Google,
        DriverKind::GenericPty | DriverKind::ShellPty => true,
    }
}

fn validate_paths(request: &MaterializeRequest<'_>) -> DriverResult<()> {
    if !Path::new(&request.spec.cwd).is_absolute() {
        return Err(DriverError::InvalidLaunchSpec(
            "cwd must be an absolute path".into(),
        ));
    }
    if !request.native_home.is_absolute() {
        return Err(DriverError::InvalidLaunchSpec(
            "native home must be an absolute path".into(),
        ));
    }
    if !request.launch_dir.is_absolute() {
        return Err(DriverError::InvalidLaunchSpec(
            "launch dir must be an absolute path".into(),
        ));
    }
    if matches!(
        request.spec.driver,
        DriverKind::ClaudePrint | DriverKind::ClaudePty
    ) {
        validate_session_uuid(request.session.session_id())?;
    }
    Ok(())
}

fn validate_session_uuid(value: &str) -> DriverResult<()> {
    uuid::Uuid::parse_str(value)
        .map_err(|_| DriverError::InvalidLaunchSpec(format!("session id {value} is not a UUID")))?;
    Ok(())
}

fn setting_sources(request: &MaterializeRequest<'_>) -> DriverResult<Vec<String>> {
    let sources = request
        .setting_sources
        .clone()
        .unwrap_or_else(|| vec!["user".into(), "project".into(), "local".into()]);
    if sources.is_empty() || sources.iter().all(|s| s.trim().is_empty()) {
        return Err(DriverError::NativeFeatureDisabled(
            "empty --setting-sources is prohibited".into(),
        ));
    }
    Ok(sources)
}

fn pin_source(source: &BinarySource) -> DriverResult<BinaryPin> {
    match source {
        BinarySource::Pinned(pin) => Ok(pin.clone()),
        BinarySource::Path(path) => pin_binary(path),
        BinarySource::Command(name) => pin_binary(name),
    }
}

fn resolve_model(spec: &InstanceSpec, profile: &ProviderProfile) -> DriverResult<String> {
    if let Some(model) = spec.model_id.clone() {
        return Ok(model);
    }
    profile.models.first().cloned().ok_or_else(|| {
        DriverError::InvalidLaunchSpec("no model_id and profile has an empty models list".into())
    })
}

fn permission_plan(
    spec: &InstanceSpec,
    origin: LaunchOrigin,
) -> DriverResult<(RecipePermission, Vec<String>, ApprovalAuthority)> {
    match &spec.permission_mode {
        PermissionMode::Claude(claude) => {
            if matches!(
                claude.mode,
                ClaudePermissionMode::BypassPermissions | ClaudePermissionMode::DontAsk
            ) && origin == LaunchOrigin::Bot
            {
                return Err(DriverError::BypassNotAllowedForBot);
            }
            let (cli, dont_ask) = map_claude_mode(claude.mode);
            let mut debt = Vec::new();
            let bypass = claude.mode == ClaudePermissionMode::BypassPermissions;
            let (prompts, extra_flags, authority) = match spec.driver {
                DriverKind::ClaudePrint if bypass => (
                    None,
                    vec!["--allow-dangerously-skip-permissions".into()],
                    ApprovalAuthority::Unknown,
                ),
                DriverKind::ClaudePrint if dont_ask => {
                    debt.push(TECH_DEBT_M0_PERM_01.to_string());
                    (Some("none".into()), vec![], ApprovalAuthority::Unknown)
                }
                DriverKind::ClaudePrint => {
                    (Some("host".into()), vec![], ApprovalAuthority::RuntimeHost)
                }
                DriverKind::ClaudePty | DriverKind::ClaudeBg if bypass => (
                    None,
                    vec!["--dangerously-skip-permissions".into()],
                    ApprovalAuthority::Unknown,
                ),
                DriverKind::ClaudePty | DriverKind::ClaudeBg => {
                    if dont_ask {
                        debt.push(TECH_DEBT_M0_PERM_01.to_string());
                    }
                    (None, vec![], ApprovalAuthority::NativeTty)
                }
                _ => (None, vec![], ApprovalAuthority::Unknown),
            };
            let cli_mode =
                if matches!(spec.driver, DriverKind::ClaudePty | DriverKind::ClaudeBg) && bypass {
                    None
                } else {
                    Some(cli.to_string())
                };
            Ok((
                RecipePermission {
                    cli_mode,
                    prompts,
                    extra_flags,
                },
                debt,
                authority,
            ))
        }
        PermissionMode::Codex(_)
        | PermissionMode::Grok(_)
        | PermissionMode::Agy(_)
        | PermissionMode::Generic(_) => Ok((
            RecipePermission {
                cli_mode: None,
                prompts: None,
                extra_flags: vec![],
            },
            vec![],
            ApprovalAuthority::Unknown,
        )),
    }
}

fn map_claude_mode(mode: ClaudePermissionMode) -> (&'static str, bool) {
    match mode {
        ClaudePermissionMode::Manual => ("default", false),
        ClaudePermissionMode::Auto => ("auto", false),
        ClaudePermissionMode::AcceptEdits => ("acceptEdits", false),
        ClaudePermissionMode::DontAsk => ("dontAsk", true),
        ClaudePermissionMode::Plan => ("plan", false),
        ClaudePermissionMode::BypassPermissions => ("bypassPermissions", false),
    }
}

fn claude_settings_json(
    profile: &ProviderProfile,
    model: &str,
    written_helper: Option<&Path>,
) -> DriverResult<Value> {
    if profile.base_url.trim().is_empty() {
        return Err(DriverError::InvalidLaunchSpec(
            "gateway delegation requires a base_url".into(),
        ));
    }
    let mut env = BTreeMap::new();
    env.insert(
        "ANTHROPIC_BASE_URL".to_string(),
        Value::String(profile.base_url.clone()),
    );
    env.insert(
        "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY".to_string(),
        Value::String("1".into()),
    );
    let mut object = json!({
        "model": model,
        "env": env,
    });
    let helper = written_helper
        .map(|path| path.to_string_lossy().into_owned())
        .or_else(|| {
            profile
                .secret_ref
                .as_ref()
                .and_then(SecretRef::helper_command)
                .map(str::to_string)
        });
    if let Some(helper) = helper {
        if !Path::new(&helper).is_absolute() {
            return Err(DriverError::InvalidLaunchSpec(
                "apiKeyHelper command must be an absolute path".into(),
            ));
        }
        object
            .as_object_mut()
            .ok_or_else(|| DriverError::InvalidLaunchSpec("settings object".into()))?
            .insert("apiKeyHelper".into(), Value::String(helper));
    }
    Ok(object)
}

const API_KEY_HELPER_TEMPLATE: &str = r#"#!/usr/bin/env python3
# Remuda apiKeyHelper. stdout is the secret only.
import json
import socket
import sys

SOCK = __SOCK__
REQ = __REQ__

def main():
    conn = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    conn.connect(SOCK)
    conn.sendall((json.dumps(REQ) + "\n").encode())
    conn.shutdown(socket.SHUT_WR)
    buf = b""
    while True:
        chunk = conn.recv(4096)
        if not chunk:
            break
        buf += chunk
    conn.close()
    line = buf.split(b"\n", 1)[0]
    resp = json.loads(line.decode())
    if not resp.get("ok"):
        print(resp.get("error") or "broker denied", file=sys.stderr)
        sys.exit(1)
    secret = resp.get("secret") or ""
    if not secret:
        print("broker omitted the secret", file=sys.stderr)
        sys.exit(1)
    if not secret.endswith("\n"):
        secret += "\n"
    sys.stdout.write(secret)

if __name__ == "__main__":
    main()
"#;

/// Render the Claude `apiKeyHelper` script for `broker` + `secret_ref`.
///
/// The script contains the per-instance token and socket path, never the API
/// key. Callers must write it `0700` and must not log the bytes.
pub fn render_api_key_helper_script(
    broker: &TokenBrokerBind,
    secret_ref: &SecretRef,
) -> DriverResult<String> {
    validate_token_broker_bind(broker)?;
    let request = serde_json::json!({
        "instanceId": broker.instance_id,
        "token": broker.token,
        "secretRef": secret_ref.as_str(),
    });
    let sock = serde_json::to_string(&broker.socket_path.to_string_lossy())?;
    let req = serde_json::to_string(&request)?;
    Ok(API_KEY_HELPER_TEMPLATE
        .replace("__SOCK__", &sock)
        .replace("__REQ__", &req))
}

fn validate_token_broker_bind(broker: &TokenBrokerBind) -> DriverResult<()> {
    if !broker.socket_path.is_absolute() {
        return Err(DriverError::InvalidLaunchSpec(
            "token broker socket must be an absolute path".into(),
        ));
    }
    if broker.instance_id.is_empty() || broker.instance_id.len() > 128 {
        return Err(DriverError::InvalidLaunchSpec(
            "token broker instance id must be 1..=128 bytes".into(),
        ));
    }
    if broker.token.len() < 16 || broker.token.len() > 128 {
        return Err(DriverError::InvalidLaunchSpec(
            "token broker instance token must be 16..=128 bytes".into(),
        ));
    }
    Ok(())
}

fn maybe_write_api_key_helper(
    request: &MaterializeRequest<'_>,
    broker: &TokenBrokerBind,
    files: &mut Vec<MaterializedFile>,
) -> DriverResult<Option<PathBuf>> {
    let Some(secret_ref) = request.profile.secret_ref.as_ref() else {
        return Ok(None);
    };
    if secret_ref.helper_command().is_some() {
        return Ok(None);
    }
    let script = render_api_key_helper_script(broker, secret_ref)?;
    let path = request.launch_dir.join("api-key-helper");
    write_private_file_with_mode(&path, script.as_bytes(), 0o700)?;
    let digest = crate::binary::hash_bytes(script.as_bytes())?;
    files.push(MaterializedFile {
        path: path.to_string_lossy().into_owned(),
        role: FileRole::ApiKeyHelper,
        mode: "0700".into(),
        content_digest: digest,
        lifetime: FileLifetime::Launch,
    });
    Ok(Some(path))
}

fn claude_argv(
    driver: DriverKind,
    permission: &RecipePermission,
    setting_sources: &[String],
    settings_path: Option<&Path>,
    model: &str,
    session: &SessionAction,
    extras: &[String],
) -> DriverResult<Vec<String>> {
    let mut argv = Vec::new();
    match driver {
        DriverKind::ClaudePrint => {
            argv.extend([
                "-p".into(),
                "--input-format".into(),
                "stream-json".into(),
                "--output-format".into(),
                "stream-json".into(),
                "--verbose".into(),
                "--include-partial-messages".into(),
                "--include-hook-events".into(),
                "--forward-subagent-text".into(),
                "--replay-user-messages".into(),
            ]);
        }
        DriverKind::ClaudeBg => argv.push("--bg".into()),
        DriverKind::ClaudePty => {}
        _ => {}
    }
    if let Some(mode) = &permission.cli_mode {
        argv.push("--permission-mode".into());
        argv.push(mode.clone());
    }
    if let Some(prompts) = &permission.prompts {
        argv.push("--permission-prompts".into());
        argv.push(prompts.clone());
        if prompts == "host" {
            argv.push("--permission-prompt-tool".into());
            argv.push("stdio".into());
        }
    }
    argv.extend(permission.extra_flags.iter().cloned());
    argv.push("--setting-sources".into());
    argv.push(setting_sources.join(","));
    if let Some(settings_path) = settings_path {
        argv.push("--settings".into());
        argv.push(settings_path.to_string_lossy().into_owned());
    }
    argv.push("--model".into());
    argv.push(model.to_string());
    if driver != DriverKind::ClaudeBg {
        match session {
            SessionAction::New { session_id } => {
                argv.push("--session-id".into());
                argv.push(session_id.clone());
            }
            SessionAction::Resume { session_id } => {
                argv.push("--resume".into());
                argv.push(session_id.clone());
            }
        }
    }
    for token in extras {
        if crate::flags::is_banned_flag_token(token)
            || token == "--bare"
            || token == "--safe-mode"
            || token == "--no-session-persistence"
        {
            return Err(DriverError::NativeFeatureDisabled(format!(
                "refusing prohibited flag {token}"
            )));
        }
    }
    argv.extend(extras.iter().cloned());
    if argv.iter().any(|token| {
        matches!(
            token.as_str(),
            "--bare" | "--safe-mode" | "--no-session-persistence" | "--continue"
        )
    }) {
        return Err(DriverError::NativeFeatureDisabled(
            "materialized argv contained a prohibited flag".into(),
        ));
    }
    Ok(argv)
}

fn reject_spec_env(spec: &InstanceSpec) -> DriverResult<()> {
    for (name, binding) in &spec.env {
        let literal = match binding {
            EnvBinding::Literal(literal) => Some(literal.value.as_str()),
            EnvBinding::Credential(_) | EnvBinding::HostEnv(_) => None,
        };
        reject_banned_env(name, literal)?;
    }
    Ok(())
}

fn collect_spec_env(
    spec: &InstanceSpec,
    delegation: Delegation,
    allowlist: &mut Vec<EnvAllowlistEntry>,
) -> DriverResult<()> {
    for (name, binding) in &spec.env {
        if delegation == Delegation::None && is_anthropic_env(name) {
            continue;
        }
        let (source, secret_ref) = match binding {
            EnvBinding::Literal(_) => (EnvAllowlistSource::Literal, None),
            EnvBinding::Credential(cred) => (
                EnvAllowlistSource::Credential,
                Some(cred.credential_ref.to_string()),
            ),
            EnvBinding::HostEnv(_) => (EnvAllowlistSource::HostEnv, None),
        };
        if allowlist.iter().any(|entry| entry.name == *name) {
            continue;
        }
        push_env(allowlist, name, source, secret_ref);
    }
    Ok(())
}

fn is_anthropic_env(name: &str) -> bool {
    name == "ANTHROPIC_BASE_URL"
        || name == "ANTHROPIC_AUTH_TOKEN"
        || name == "ANTHROPIC_API_KEY"
        || name.starts_with("ANTHROPIC_")
}

fn push_env(
    allowlist: &mut Vec<EnvAllowlistEntry>,
    name: &str,
    source: EnvAllowlistSource,
    secret_ref: Option<String>,
) {
    if allowlist.iter().any(|entry| entry.name == name) {
        return;
    }
    allowlist.push(EnvAllowlistEntry {
        name: name.to_string(),
        source,
        secret_ref,
    });
}

fn redact_argv(argv: &[String], files: &[MaterializedFile]) -> Vec<String> {
    argv.iter()
        .map(|token| {
            if files.iter().any(|file| file.path == *token) {
                "<settings>".to_string()
            } else {
                token.clone()
            }
        })
        .collect()
}

fn write_private_file(path: &Path, contents: &[u8]) -> DriverResult<()> {
    write_private_file_with_mode(path, contents, 0o600)
}

fn write_private_file_with_mode(path: &Path, contents: &[u8], mode: u32) -> DriverResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        set_dir_mode(parent, 0o700)?;
    }
    let tmp = {
        let mut raw = path.as_os_str().to_os_string();
        raw.push(".tmp");
        PathBuf::from(raw)
    };
    {
        let mut opts = OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(mode);
        }
        let mut file = opts.open(&tmp)?;
        file.write_all(contents)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    set_file_mode(path, mode)?;
    Ok(())
}

fn set_dir_mode(path: &Path, mode: u32) -> DriverResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    let _ = mode;
    let _ = path;
    Ok(())
}

fn set_file_mode(path: &Path, mode: u32) -> DriverResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    let _ = mode;
    let _ = path;
    Ok(())
}
