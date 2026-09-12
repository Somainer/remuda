//! Turn `InstanceSpec` + [`ProviderProfile`] into a serializable [`LaunchRecipe`].

use crate::binary::{BinaryPin, pin_binary};
use crate::error::{DriverError, DriverResult};
use crate::flags::{reject_banned_env, validate_spec_args};
use crate::profile::{ProviderHealth, ProviderKind, ProviderProfile};
use crate::recipe::{
    EnvAllowlistEntry, EnvAllowlistSource, FileLifetime, FileRole, LaunchAudit, LaunchRecipe,
    MaterializedFile, RecipePermission, RecipeProvider, TECH_DEBT_M0_PERM_01,
};
use remuda_protocol::{
    AgentKind, ApprovalAuthority, BoolLiteral, ClaudePermissionMode, DriverKind, EnvBinding, Id,
    InputDelivery, InstanceSpec, PermissionMode,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
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
}

/// Materialize `spec` + `profile` into a durable recipe and 0600 overlay files.
///
/// Idempotent for identical [`MaterializeRequest`] values: overlay bytes, argv,
/// and pin are stable. Secret values and prompts are never written.
pub fn materialize(request: &MaterializeRequest<'_>) -> DriverResult<LaunchRecipe> {
    validate_spec_profile(request.spec, request.profile)?;
    validate_paths(request)?;
    let extras = validate_spec_args(request.spec.driver, &request.spec.args)?;
    reject_spec_env(request.spec)?;
    let setting_sources = setting_sources(request)?;
    let binary = pin_source(&request.binary)?;
    let model = resolve_model(request.spec, request.profile)?;
    let (permission, debt, approval) = permission_plan(request.spec)?;

    fs::create_dir_all(&request.launch_dir)?;
    set_dir_mode(&request.launch_dir, 0o700)?;

    let mut files = Vec::new();
    let mut env_allowlist = Vec::new();
    let mut argv;
    let input_delivery;
    let session_id;

    match request.spec.driver {
        DriverKind::ClaudePrint | DriverKind::ClaudePty | DriverKind::ClaudeBg => {
            let settings_path = request.launch_dir.join("settings.json");
            let settings = claude_settings_json(request.profile, &model)?;
            let bytes = serde_json::to_vec_pretty(&settings)?;
            write_private_file(&settings_path, &bytes)?;
            let digest = crate::binary::hash_bytes(&bytes)?;
            files.push(MaterializedFile {
                path: settings_path.to_string_lossy().into_owned(),
                role: FileRole::Settings,
                mode: "0600".into(),
                content_digest: digest.clone(),
                lifetime: FileLifetime::Launch,
            });
            argv = claude_argv(
                request.spec.driver,
                &permission,
                &setting_sources,
                &settings_path,
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
            push_env(
                &mut env_allowlist,
                "ANTHROPIC_BASE_URL",
                EnvAllowlistSource::ProviderOverlay,
                None,
            );
            if let Some(helper) = request.profile.secret_ref.helper_command() {
                let _ = helper;
            } else {
                push_env(
                    &mut env_allowlist,
                    "ANTHROPIC_AUTH_TOKEN",
                    EnvAllowlistSource::Credential,
                    Some(request.profile.secret_ref.as_str().to_string()),
                );
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
            push_env(
                &mut env_allowlist,
                "RUNTIME_PROVIDER_TOKEN",
                EnvAllowlistSource::Credential,
                Some(request.profile.secret_ref.as_str().to_string()),
            );
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
            push_env(
                &mut env_allowlist,
                "RUNTIME_PROVIDER_TOKEN",
                EnvAllowlistSource::Credential,
                Some(request.profile.secret_ref.as_str().to_string()),
            );
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
            push_env(
                &mut env_allowlist,
                "GEMINI_API_KEY",
                EnvAllowlistSource::Credential,
                Some(request.profile.secret_ref.as_str().to_string()),
            );
        }
        DriverKind::GenericPty => {
            argv = extras;
            input_delivery = InputDelivery::Tty;
            session_id = None;
        }
    }

    collect_spec_env(request.spec, &mut env_allowlist)?;

    let settings_digest = files
        .iter()
        .find(|file| file.role == FileRole::Settings)
        .map(|file| file.content_digest.clone());
    let redacted_argv = redact_argv(&argv, &files);
    let env_names = env_allowlist.iter().map(|e| e.name.clone()).collect();
    let credential_refs = env_allowlist
        .iter()
        .filter_map(|e| e.secret_ref.clone())
        .collect();

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
            secret_ref: request.profile.secret_ref.as_str().to_string(),
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
    if spec.kind != agent_kind(spec.driver) {
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
        DriverKind::GenericPty => true,
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
) -> DriverResult<(RecipePermission, Vec<String>, ApprovalAuthority)> {
    match &spec.permission_mode {
        PermissionMode::Claude(claude) => {
            let (cli, dont_ask) = map_claude_mode(claude.mode);
            let mut debt = Vec::new();
            let (prompts, authority) = match spec.driver {
                DriverKind::ClaudePrint if dont_ask => {
                    debt.push(TECH_DEBT_M0_PERM_01.to_string());
                    (Some("none".into()), ApprovalAuthority::Unknown)
                }
                DriverKind::ClaudePrint => (Some("host".into()), ApprovalAuthority::RuntimeHost),
                DriverKind::ClaudePty | DriverKind::ClaudeBg => {
                    if dont_ask {
                        debt.push(TECH_DEBT_M0_PERM_01.to_string());
                    }
                    (None, ApprovalAuthority::NativeTty)
                }
                _ => (None, ApprovalAuthority::Unknown),
            };
            Ok((
                RecipePermission {
                    cli_mode: Some(cli.to_string()),
                    prompts,
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

fn claude_settings_json(profile: &ProviderProfile, model: &str) -> DriverResult<Value> {
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
    if let Some(helper) = profile.secret_ref.helper_command() {
        if !Path::new(helper).is_absolute() {
            return Err(DriverError::InvalidLaunchSpec(
                "apiKeyHelper command must be an absolute path".into(),
            ));
        }
        object
            .as_object_mut()
            .ok_or_else(|| DriverError::InvalidLaunchSpec("settings object".into()))?
            .insert("apiKeyHelper".into(), Value::String(helper.to_string()));
    }
    Ok(object)
}

fn claude_argv(
    driver: DriverKind,
    permission: &RecipePermission,
    setting_sources: &[String],
    settings_path: &Path,
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
    argv.push("--setting-sources".into());
    argv.push(setting_sources.join(","));
    argv.push("--settings".into());
    argv.push(settings_path.to_string_lossy().into_owned());
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
    allowlist: &mut Vec<EnvAllowlistEntry>,
) -> DriverResult<()> {
    for (name, binding) in &spec.env {
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
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        set_dir_mode(parent, 0o700)?;
    }
    let tmp = path.with_extension("json.tmp");
    {
        let mut opts = OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&tmp)?;
        file.write_all(contents)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    set_file_mode(path, 0o600)?;
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
