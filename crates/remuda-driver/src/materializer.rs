//! Turn `InstanceSpec` + [`ProviderProfile`] into a serializable [`LaunchRecipe`].

use crate::binary::{BinaryPin, pin_binary};
use crate::error::{DriverError, DriverResult};
use crate::flags::{reject_banned_env, validate_spec_args};
use crate::profile::{
    Delegation, ProviderHealth, ProviderKind, ProviderProfile, SecretRef, SecretRefPolicy,
};
use crate::recipe::{
    EnvAllowlistEntry, EnvAllowlistSource, FileLifetime, FileRole, LaunchAudit, LaunchRecipe,
    MaterializedFile, RecipePermission, RecipeProvider, TECH_DEBT_M0_PERM_01,
};
use remuda_protocol::{
    AgentKind, AgyPermissionMode, ApprovalAuthority, ApprovalPolicy, BoolLiteral,
    ClaudePermissionMode, CodexExecution, CommandOrigin, DriverKind, EnvBinding,
    GrokPermissionMode, Id, InputDelivery, InputOrigin, InstanceSpec, PermissionMode,
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
    /// Optional explicit `--setting-sources`; omitted for normal source loading.
    pub setting_sources: Option<Vec<String>>,
    /// Who originated this launch. Bot/dispatcher specs cannot request bypass/yolo.
    pub origin: LaunchOrigin,
    /// Whether `native_home` is a Remuda-managed home this launch may write the
    /// skill tree into. `None` reads as managed (the default); `Some(false)` is
    /// the inherited operator home, which is never written (D-045).
    pub native_home_managed: Option<bool>,
    /// Host-validated `--settings` overlay. Contents are never logged.
    pub settings_overlay_path: Option<PathBuf>,
    /// Where `file:` / `helper:` secret refs are allowed to point (S3, S4).
    ///
    /// `None` refuses a profile-supplied `helper:` ref outright. A helper Remuda
    /// writes itself is unaffected — that path and its 0700 mode are ours.
    pub secret_policy: Option<SecretRefPolicy>,
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
    /// Authenticated human UI/CLI.
    Human,
    /// Bot or dispatcher path. Cannot request bypass (D-011).
    Bot,
    /// An instance, or an unknown caller. Never inherits operator authority.
    #[default]
    #[serde(other)]
    Agent,
}

impl From<InputOrigin> for LaunchOrigin {
    fn from(value: InputOrigin) -> Self {
        match value {
            InputOrigin::Human => Self::Human,
            InputOrigin::Bot => Self::Bot,
            InputOrigin::Agent => Self::Agent,
        }
    }
}

impl From<CommandOrigin> for LaunchOrigin {
    fn from(value: CommandOrigin) -> Self {
        match value {
            CommandOrigin::Bot => Self::Bot,
            CommandOrigin::Ui | CommandOrigin::Cli => Self::Human,
            CommandOrigin::Mcp | CommandOrigin::System => Self::Agent,
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
    reject_agent_launch_overrides(request.spec, request.origin)?;
    // D-045 gate half: refuse unknown capability values, agent-originated
    // grants, bypass+computer-use and unsupported kinds BEFORE any file is
    // written. The grant itself is materialized below, once launch_dir exists.
    crate::launch::skills::computer_use_requested(
        &request.spec.capabilities,
        request.origin,
        &request.spec.permission_mode,
        request.spec.kind,
    )?;
    // D-045 leg (b) collision: refuse before any file is written, in both
    // `--mcp-config path` and `--mcp-config=path` forms.
    crate::launch::skills::reject_caller_mcp_config(
        &request.spec.capabilities,
        &request.spec.args,
    )?;
    let setting_sources = setting_sources(request)?;
    // M2: the override is validated before any file is written, so a bad path
    // fails the launch without leaving an overlay or a shim behind.
    let binary_override = binary_override_pin(request)?;
    let binary = match &binary_override {
        Some(pin) => pin.clone(),
        None => pin_source(&request.binary)?,
    };
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

    // D-028 §5.1: `shell-pty` is no longer a materializer dead end. Which
    // recipe it gets depends on `spec.kind`, because one driver now carries
    // two very different launches: `terminal` / `generic` is a login shell,
    // and `claude` / `codex` / `grok` / `agy` is an agent in a native PTY.
    if request.spec.driver == DriverKind::ShellPty && is_agent_kind(request.spec.kind) {
        return materialize_shell_pty_agent(
            request,
            extras,
            setting_sources,
            binary,
            binary_override.is_some(),
        );
    }

    match request.spec.driver {
        DriverKind::ClaudePrint
        | DriverKind::ClaudeSdk
        | DriverKind::ClaudePty
        | DriverKind::ClaudeBg => {
            let user_overlay = match request.settings_overlay_path.as_ref() {
                Some(path) => Some(validate_settings_overlay(path)?),
                None => None,
            };
            let settings_path = if let Some(path) = user_overlay.clone() {
                let bytes = fs::read(&path)?;
                let digest = crate::binary::hash_bytes(&bytes)?;
                files.push(MaterializedFile {
                    path: path.to_string_lossy().into_owned(),
                    role: FileRole::Settings,
                    mode: "0600".into(),
                    content_digest: digest,
                    lifetime: FileLifetime::NativeStore,
                });
                Some(path)
            } else if inject_provider {
                if let Some(bind) = broker {
                    api_key_helper_path = maybe_write_api_key_helper(request, bind, &mut files)?;
                }
                let path = request.launch_dir.join("settings.json");
                let settings = claude_settings_json(
                    request.profile,
                    &model,
                    api_key_helper_path.as_deref(),
                    request.secret_policy.as_ref(),
                )?;
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
                &ClaudeArgv {
                    permission: &permission,
                    setting_sources: request.setting_sources.as_deref().unwrap_or(&[]),
                    settings_path: settings_path.as_deref(),
                    model: &model,
                    effort: request.spec.effort,
                    session: &request.session,
                    extras: &extras,
                },
            )?;
            input_delivery = match request.spec.driver {
                // Both stream-json carriers take turns as `user` NDJSON lines on
                // the child's stdin; sdk simply keeps that stdin open (§2.1).
                DriverKind::ClaudePrint | DriverKind::ClaudeSdk => InputDelivery::Stdio,
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
                if !request.profile.base_url.trim().is_empty() {
                    push_env(
                        &mut env_allowlist,
                        "ANTHROPIC_BASE_URL",
                        EnvAllowlistSource::ProviderOverlay,
                        None,
                    );
                }
                match request.profile.secret_ref.as_ref() {
                    Some(secret)
                        if secret.helper_command().is_some() || api_key_helper_path.is_some() => {}
                    Some(secret) => push_env(
                        &mut env_allowlist,
                        "ANTHROPIC_AUTH_TOKEN",
                        EnvAllowlistSource::Credential,
                        Some(secret.as_str().to_string()),
                    ),
                    None if user_overlay.is_some() => {}
                    None => {
                        return Err(DriverError::InvalidLaunchSpec(
                            "gateway delegation requires a settings overlay or secret_ref".into(),
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
            // A granted codex/grok needs its per-kind home pinned because its
            // D-045 `[mcp_servers]` (or, for grok, the neutralised shadow)
            // lives there. Only on grant: an ordinary generic-pty agent keeps
            // inheriting the host's own home so its config/auth still work.
            let granted = !request.spec.capabilities.is_empty();
            if granted && request.spec.kind == AgentKind::Codex {
                push_env(
                    &mut env_allowlist,
                    "CODEX_HOME",
                    EnvAllowlistSource::NativeHome,
                    None,
                );
            } else if granted && request.spec.kind == AgentKind::Grok {
                push_env(
                    &mut env_allowlist,
                    "GROK_HOME",
                    EnvAllowlistSource::NativeHome,
                    None,
                );
            }
        }
        DriverKind::ShellPty => {
            // Login `$SHELL` under kind `terminal` / `generic`. The agent
            // kinds took the `materialize_shell_pty_agent` branch above.
            argv = extras;
            input_delivery = InputDelivery::Tty;
            session_id = None;
        }
    }

    collect_spec_env(request.spec, request.profile.delegation, &mut env_allowlist)?;

    let (capabilities, mcp_servers) =
        apply_capability_grant(request, &mut argv, &mut files, &mut env_allowlist)?;

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
            model_pin: pinned_model(request.spec),
        },
        permission,
        capabilities,
        mcp_servers,
        technical_debt: debt,
        audit: LaunchAudit {
            env_names,
            credential_refs,
            redacted_argv,
            settings_digest,
            prohibited_options_checked: BoolLiteral,
            binary_override: binary_override.is_some(),
            approval_authority: approval,
        },
    };
    tracing::info!(
        launch_id = %recipe.launch_id,
        driver = ?recipe.driver,
        binary = %recipe.binary.abs_path,
        binary_override = recipe.audit.binary_override,
        binary_digest = %String::from(recipe.binary.sha256.clone()),
        debt = ?recipe.technical_debt,
        "materialized launch recipe"
    );
    Ok(recipe)
}

/// Kinds that name an agent CLI (everything but a bare shell).
fn is_agent_kind(kind: AgentKind) -> bool {
    matches!(
        kind,
        AgentKind::Claude | AgentKind::Codex | AgentKind::Grok | AgentKind::Agy
    )
}

/// D-045: materialize the granted capability into argv, the launch dir and
/// (for claude) a managed home; push the handshake env; return what the recipe
/// must audit. Does nothing — and writes nothing — when no capability is
/// requested. Refusals happen before any write.
#[allow(clippy::needless_pass_by_value)]
fn apply_capability_grant(
    request: &MaterializeRequest<'_>,
    argv: &mut Vec<String>,
    files: &mut Vec<MaterializedFile>,
    env_allowlist: &mut Vec<EnvAllowlistEntry>,
) -> DriverResult<(Vec<String>, Vec<crate::recipe::GrantedMcpServer>)> {
    let Some(grant) =
        crate::launch::skills::materialize_grant(&crate::launch::skills::GrantRequest {
            capabilities: &request.spec.capabilities,
            origin: request.origin,
            permission: &request.spec.permission_mode,
            kind: request.spec.kind,
            launch_dir: &request.launch_dir,
            native_home: &request.native_home,
            native_home_managed: request.native_home_managed.unwrap_or(true),
        })?
    else {
        return Ok((Vec::new(), Vec::new()));
    };
    if !grant.argv.is_empty() {
        argv.extend(grant.argv);
    }
    // The handshake name/value is defined by the grant in one place; the value
    // rides the allowlist entry (Credential-shaped value slot) rather than
    // being hardcoded at the spawn sites.
    let (name, value) = grant.env;
    push_env(
        env_allowlist,
        name,
        EnvAllowlistSource::Capability,
        Some(value.to_owned()),
    );
    files.extend(grant.files);
    Ok((grant.capabilities, vec![grant.mcp_server]))
}

/// D-028 §5.1: recipe for an agent CLI running in a Remuda-owned native PTY.
///
/// Unlike the Herdr path this produces a *complete* recipe — real env
/// allowlist, real settings digest, real provider kind — because §5.1 step 4
/// makes those an audit requirement rather than the stub `shell_pty.rs` used
/// to emit. argv comes from the per-kind preset in [`crate::presets`], not
/// from passing `spec.args` straight to the command builder.
fn materialize_shell_pty_agent(
    request: &MaterializeRequest<'_>,
    extras: Vec<String>,
    setting_sources: Vec<String>,
    binary: BinaryPin,
    binary_override: bool,
) -> DriverResult<LaunchRecipe> {
    let preset = crate::presets::preset_for_spec(request.spec)?;
    let (permission, debt, approval) = permission_plan(request.spec, request.origin)?;
    crate::launch::skills::computer_use_requested(
        &request.spec.capabilities,
        request.origin,
        &request.spec.permission_mode,
        request.spec.kind,
    )?;
    // D-045 leg (b) collision: refuse before any file is written, in both
    // `--mcp-config path` and `--mcp-config=path` forms.
    crate::launch::skills::reject_caller_mcp_config(
        &request.spec.capabilities,
        &request.spec.args,
    )?;

    fs::create_dir_all(&request.launch_dir)?;
    set_dir_mode(&request.launch_dir, 0o700)?;

    let mut files = Vec::new();
    let mut env_allowlist = Vec::new();

    // The overlay slot. A caller-supplied path is honoured. The native PTY
    // agent launch never traverses the PATH shim that injects the generated
    // overlay, so the hook session's `<launch_dir>/settings.json` (hook
    // commands + TUI flags, written when `REMUDA_PTY_HOOKS=1`) is picked up
    // here explicitly. Nothing is invented when the file is absent: hooks off
    // means no `--settings` flag, not an empty file shadowing user settings.
    let overlay_path = request
        .settings_overlay_path
        .as_deref()
        .map(validate_settings_overlay)
        .transpose()?
        .or_else(|| {
            let generated = request.launch_dir.join("settings.json");
            generated.is_file().then_some(generated)
        });
    let overlay = match overlay_path {
        Some(path) => {
            let bytes = fs::read(&path)?;
            let digest = crate::binary::hash_bytes(&bytes)?;
            files.push(MaterializedFile {
                path: path.to_string_lossy().into_owned(),
                role: FileRole::Settings,
                mode: "0600".into(),
                content_digest: digest,
                lifetime: FileLifetime::Launch,
            });
            Some(path)
        }
        None => None,
    };

    let mut argv: Vec<String> = Vec::new();
    if preset.settings_flag {
        if request.setting_sources.is_some() {
            argv.push("--setting-sources".into());
            argv.push(setting_sources.join(","));
        }
        if let Some(path) = overlay.as_ref() {
            argv.push("--settings".into());
            argv.push(path.to_string_lossy().into_owned());
        }
    }
    // model-pin-1: an explicit pin must reach the process on THIS path
    // too. It used to reach only `claude_argv` (the print/pty/bg carriers), so a
    // `dispatch --model` on driver shell-pty was recorded as honoured in
    // `provider.model_requested` below and in the Hub roster while the host's
    // own default silently answered every turn.
    //
    // Only `spec.model_id` — never `resolve_model`, whose profile fallback would
    // mint a pin nobody asked for (brief rule 3: no pin, no token). The id is
    // passed byte-identical: a `[1m]` context suffix is part of the id Claude
    // parses, so trimming or normalising it would select a different model.
    if let Some(flag) = preset.model_flag
        && let Some(model) = pinned_model(request.spec)
    {
        argv.push(flag.to_owned());
        argv.push(model);
    }
    // §9.1 + composer-slider-5: the selection reaches each CLI in its OWN
    // vocabulary: `--effort` for claude, `-c model_reasoning_effort=…` for
    // codex (a top-level `--effort` is a clap error in codex), canonical
    // `--reasoning-effort` for grok. Absent effort emits nothing at all rather
    // than pinning a default the user never chose.
    argv.extend(crate::effort::effort_argv(
        request.spec.kind,
        request.spec.effort,
    )?);
    match &request.session {
        SessionAction::Resume { session_id } => {
            argv.push("--resume".into());
            argv.push(session_id.clone());
        }
        // §5.6: a new agent-pty session does not pin `--session-id`; the id
        // comes back from the harness (SessionStart hook / index file), so
        // inventing one here would create a second identity to reconcile.
        SessionAction::New { .. } => {}
    }
    for token in &extras {
        if crate::flags::is_banned_flag_token(token) {
            return Err(DriverError::NativeFeatureDisabled(format!(
                "refusing prohibited flag {token}"
            )));
        }
    }
    // Effort is emitted per-kind just above; a caller-supplied duplicate would
    // either repeat it or use the other CLI's vocabulary.
    crate::effort::ensure_no_effort_in_extras(request.spec.kind, &extras)?;
    argv.extend(extras);
    // Per-harness permission axes: Codex's approval policy + sandbox mode,
    // Per-harness permission axes ride `permission.extra_flags` from
    // permission_plan: Codex's --ask-for-approval/--sandbox, Grok/agy yolo
    // flags. Claude PTY sessions deliberately get NO `--permission-mode`: the
    // shell/PTY launch inherits its own TUI mode (the wheel manages it in
    // session), and the flag both constrains a promoted shell and is rejected
    // by the fake harness. (The print carrier is the only path that emits the
    // Claude word, via `claude_argv`.) merge_yolo_argv handles the legacy
    // Claude-bypass spelling on top.
    for flag in &permission.extra_flags {
        if !argv.iter().any(|token| token == flag) {
            argv.push(flag.clone());
        }
    }
    crate::presets::merge_yolo_argv(
        &mut argv,
        preset,
        &request.spec.permission_mode,
        request.origin,
    );

    if let Some(name) = preset.home_env {
        push_env(
            &mut env_allowlist,
            name,
            EnvAllowlistSource::NativeHome,
            None,
        );
    }
    collect_spec_env(request.spec, request.profile.delegation, &mut env_allowlist)?;

    let (capabilities, mcp_servers) =
        apply_capability_grant(request, &mut argv, &mut files, &mut env_allowlist)?;

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

    Ok(LaunchRecipe {
        launch_id: request.launch_id.clone(),
        driver: DriverKind::ShellPty,
        binary,
        cwd: request.spec.cwd.clone(),
        argv,
        env_allowlist,
        materialized_files: files,
        setting_sources: if preset.settings_flag {
            setting_sources
        } else {
            vec![]
        },
        session_id: match &request.session {
            SessionAction::Resume { session_id } => Some(session_id.clone()),
            SessionAction::New { .. } => None,
        },
        native_home: request.native_home.to_string_lossy().into_owned(),
        input_delivery: InputDelivery::Tty,
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
            model_requested: resolve_model(request.spec, request.profile).unwrap_or_default(),
            model_pin: pinned_model(request.spec),
        },
        permission,
        capabilities,
        mcp_servers,
        technical_debt: debt,
        audit: LaunchAudit {
            env_names,
            credential_refs,
            redacted_argv,
            settings_digest,
            prohibited_options_checked: BoolLiteral,
            binary_override,
            // The agent owns its own TUI prompts until the hook adjudication
            // path (D-028 §4.4 tier A) is wired; claiming runtime-hook here
            // before then would be a lie in the audit record.
            approval_authority: match approval {
                ApprovalAuthority::RuntimeHost => ApprovalAuthority::NativeTty,
                other => other,
            },
        },
    })
}

fn validate_spec_profile(spec: &InstanceSpec, profile: &ProviderProfile) -> DriverResult<()> {
    if spec.driver == DriverKind::ShellPty {
        // Both shapes are legal for this driver now (D-028 §5.1): a login
        // shell under `terminal` / `generic`, or an agent CLI under its own
        // kind. Every kind is therefore accepted, and the kind picks the arm.
    } else if spec.driver == DriverKind::GenericPty {
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
        DriverKind::ClaudePrint
        | DriverKind::ClaudeSdk
        | DriverKind::ClaudePty
        | DriverKind::ClaudeBg => AgentKind::Claude,
        DriverKind::CodexAppserver => AgentKind::Codex,
        DriverKind::GrokAcp => AgentKind::Grok,
        DriverKind::AgyPrint => AgentKind::Agy,
        DriverKind::GenericPty => AgentKind::Generic,
        DriverKind::ShellPty => AgentKind::Terminal,
    }
}

fn provider_matches(driver: DriverKind, kind: ProviderKind) -> bool {
    match driver {
        DriverKind::ClaudePrint
        | DriverKind::ClaudeSdk
        | DriverKind::ClaudePty
        | DriverKind::ClaudeBg => kind == ProviderKind::Anthropic,
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

/// Validate and pin `spec.binary_path`, when the spec carries one.
///
/// The guard roots come from the request rather than being rediscovered here:
/// `launch_dir` is the instance's launch directory, and `cwd` is where the
/// agent will run. Both are places the agent can write, which is exactly what
/// makes naming a binary inside them a privilege escalation.
fn binary_override_pin(request: &MaterializeRequest<'_>) -> DriverResult<Option<BinaryPin>> {
    let Some(raw) = request
        .spec
        .binary_path
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    // `launch_dir` is `<instance>/launch`, so its parent is the instance dir.
    let guard = crate::binary::BinaryOverrideGuard {
        instance_dir: request
            .launch_dir
            .parent()
            .map(Path::to_path_buf)
            .or_else(|| Some(request.launch_dir.clone())),
        cwd: Some(PathBuf::from(&request.spec.cwd)),
        extra: Vec::new(),
    };
    let pin =
        crate::binary::validate_binary_override(raw, &guard, request.spec.binary_sha256.as_ref())?;
    Ok(Some(pin))
}

/// Bot and agent origins may not choose their own argv or executable.
///
/// Same shape as the bypass refusal in [`permission_plan`]: a dispatcher or an
/// instance never inherits operator authority, and both `args` and
/// `binary_path` are operator-level choices — one picks flags the allowlist
/// would otherwise have to defend alone, the other picks the code that runs.
fn reject_agent_launch_overrides(spec: &InstanceSpec, origin: LaunchOrigin) -> DriverResult<()> {
    if matches!(origin, LaunchOrigin::Human) {
        return Ok(());
    }
    if !spec.args.is_empty() {
        return Err(DriverError::InvalidLaunchSpec(format!(
            "{origin:?} origin may not set launch args"
        )));
    }
    if spec
        .binary_path
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty())
    {
        return Err(DriverError::InvalidLaunchSpec(format!(
            "{origin:?} origin may not set binaryPath"
        )));
    }
    Ok(())
}

fn resolve_model(spec: &InstanceSpec, profile: &ProviderProfile) -> DriverResult<String> {
    if let Some(model) = spec.model_id.clone() {
        return Ok(model);
    }
    profile.models.first().cloned().ok_or_else(|| {
        DriverError::InvalidLaunchSpec("no model_id and profile has an empty models list".into())
    })
}

/// The explicitly pinned model id, if this launch has one.
///
/// Deliberately *not* [`resolve_model`]: that falls back to the profile's first
/// model, which is the right answer for "what will answer this session" but the
/// wrong one for "what did the requester pin". model-pin-1 rule 3 — an
/// instance with no `model_id` keeps today's behaviour exactly (host default, no
/// argv token, no strip), so this returns `None` and every caller stays quiet.
///
/// The id is returned verbatim. A `[1m]` context-window suffix is part of the id
/// the gateway catalog lists and Claude parses; trimming it would silently pick
/// the other variant.
pub(crate) fn pinned_model(spec: &InstanceSpec) -> Option<String> {
    spec.model_id
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(str::to_owned)
}

/// [`pinned_model`] for callers outside this module (the native carrier's
/// settings merge), in the shape `Option::and_then` wants.
pub fn pinned_model_for(spec: &InstanceSpec) -> Option<String> {
    pinned_model(spec)
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
            ) && matches!(origin, LaunchOrigin::Bot | LaunchOrigin::Agent)
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
                // §2.8: sdk takes print's bypass and host paths, but not its M0
                // `dontAsk` auto-deny debt (`TD-M0-PERM-01`). Silently denying
                // every tool call is a behaviour no caller asked for, so the new
                // carrier refuses the mode instead of inheriting the debt.
                DriverKind::ClaudeSdk if bypass => (
                    None,
                    vec!["--allow-dangerously-skip-permissions".into()],
                    ApprovalAuthority::Unknown,
                ),
                DriverKind::ClaudeSdk if dont_ask => {
                    return Err(DriverError::NativeFeatureDisabled(
                        "claude-sdk does not implement the dontAsk auto-deny preset \
                         (TD-M0-PERM-01): use default (host approvals) or explicit bypass"
                            .into(),
                    ));
                }
                DriverKind::ClaudeSdk => {
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
        PermissionMode::Codex(codex) => {
            if codex.approval_policy == ApprovalPolicy::Never
                && matches!(origin, LaunchOrigin::Bot | LaunchOrigin::Agent)
            {
                return Err(DriverError::BypassNotAllowedForBot);
            }
            // Codex's two real CLI axes (`codex --help` 0.x): approval policy
            // `-a/--ask-for-approval <untrusted|on-request|never>` and sandbox
            // `-s/--sandbox <read-only|workspace-write|danger-full-access>`.
            // never + danger-full-access is exactly the vendor yolo flag.
            let mut extra_flags = vec![
                "--ask-for-approval".into(),
                match codex.approval_policy {
                    ApprovalPolicy::Untrusted => "untrusted".into(),
                    ApprovalPolicy::OnRequest => "on-request".into(),
                    ApprovalPolicy::Never => "never".into(),
                },
            ];
            if let CodexExecution::Sandbox(sandbox) = &codex.execution {
                extra_flags.push("--sandbox".into());
                extra_flags.push(match sandbox.sandbox {
                    remuda_protocol::SandboxMode::ReadOnly => "read-only".into(),
                    remuda_protocol::SandboxMode::WorkspaceWrite => "workspace-write".into(),
                    remuda_protocol::SandboxMode::DangerFullAccess => "danger-full-access".into(),
                });
            }
            Ok((
                RecipePermission {
                    cli_mode: None,
                    prompts: None,
                    extra_flags,
                },
                vec![],
                ApprovalAuthority::NativeTty,
            ))
        }
        PermissionMode::Grok(grok) => {
            if grok.mode == GrokPermissionMode::AlwaysApprove
                && matches!(origin, LaunchOrigin::Bot | LaunchOrigin::Agent)
            {
                return Err(DriverError::BypassNotAllowedForBot);
            }
            // grok agent's yolo argv; the other modes are the ACP default and
            // `_meta.autoMode` (build-gated by the native CLI), no flag.
            let extra_flags = if grok.mode == GrokPermissionMode::AlwaysApprove {
                vec!["--always-approve".into()]
            } else {
                vec![]
            };
            Ok((
                RecipePermission {
                    cli_mode: None,
                    prompts: None,
                    extra_flags,
                },
                vec![],
                ApprovalAuthority::NativeTty,
            ))
        }
        PermissionMode::Agy(agy) => {
            if agy.mode == AgyPermissionMode::AlwaysProceed
                && matches!(origin, LaunchOrigin::Bot | LaunchOrigin::Agent)
            {
                return Err(DriverError::BypassNotAllowedForBot);
            }
            let extra_flags = match agy.mode {
                AgyPermissionMode::AlwaysProceed => vec!["--yolo".into()],
                AgyPermissionMode::AcceptEdits => vec!["--accept-edits".into()],
                AgyPermissionMode::Plan => vec!["--plan".into()],
                AgyPermissionMode::Native => vec![],
            };
            Ok((
                RecipePermission {
                    cli_mode: None,
                    prompts: None,
                    extra_flags,
                },
                vec![],
                ApprovalAuthority::NativeTty,
            ))
        }
        PermissionMode::Generic(_) => Ok((
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
    policy: Option<&SecretRefPolicy>,
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
    // A helper we wrote ourselves is trusted: we chose the path and the 0700 mode.
    // A helper spelled by the profile is not — Claude runs `apiKeyHelper` as a
    // shell command, so `is_absolute()` alone admits `/bin/sh -c '…'` (S3).
    let helper = match written_helper {
        Some(path) => Some(path.to_string_lossy().into_owned()),
        None => match profile
            .secret_ref
            .as_ref()
            .and_then(SecretRef::helper_command)
        {
            Some(raw) => {
                let policy = policy.ok_or_else(|| {
                    DriverError::InvalidLaunchSpec(
                        "helper: secret refs require a configured SecretRefPolicy".into(),
                    )
                })?;
                Some(policy.resolve_helper(raw)?.to_string_lossy().into_owned())
            }
            None => None,
        },
    };
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

/// Everything `claude_argv` needs beyond the driver kind.
struct ClaudeArgv<'a> {
    permission: &'a RecipePermission,
    setting_sources: &'a [String],
    settings_path: Option<&'a Path>,
    model: &'a str,
    effort: Option<remuda_protocol::EffortSelection>,
    session: &'a SessionAction,
    extras: &'a [String],
}

fn claude_argv(driver: DriverKind, inputs: &ClaudeArgv<'_>) -> DriverResult<Vec<String>> {
    let ClaudeArgv {
        permission,
        setting_sources,
        settings_path,
        model,
        effort,
        session,
        extras,
    } = *inputs;
    let mut argv = Vec::new();
    match driver {
        // One template, two carriers. `-p` is "Print response and exit": it is
        // the only difference, and it is why print dies after one turn
        // (`print-replacement.md` §2.1, key decision 3). Non-interactive comes
        // from piped stdout, not from this flag (§1.3).
        DriverKind::ClaudePrint | DriverKind::ClaudeSdk => {
            if driver == DriverKind::ClaudePrint {
                argv.push("-p".into());
            }
            argv.extend([
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
    if !setting_sources.is_empty() {
        argv.push("--setting-sources".into());
        argv.push(setting_sources.join(","));
    }
    if let Some(settings_path) = settings_path {
        argv.push("--settings".into());
        argv.push(settings_path.to_string_lossy().into_owned());
    }
    argv.push("--model".into());
    argv.push(model.to_string());
    // §9.1: launch-time effort. `--effort ultracode` is the flag spelling for
    // the boolean; it is distinct from the Codex `ultra` level. Claude parses
    // neither `minimal` nor `ultra` — refuse rather than launch-crash.
    if let Some(effort) = effort {
        argv.extend(crate::effort::effort_argv(AgentKind::Claude, Some(effort))?);
    }
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
        // A HostEnv binding names the *source* variable in the Node's
        // environment, which need not match the map key it is injected under.
        // Check it too, or `FOO: host-env(REMUDA_BOOTSTRAP_TOKEN)` walks the
        // token straight past the key check (`security-review-2.md` S2).
        if let EnvBinding::HostEnv(host) = binding {
            reject_banned_env(&host.name, None)?;
        }
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

fn validate_settings_overlay(path: &Path) -> DriverResult<PathBuf> {
    if !path.is_absolute() {
        return Err(DriverError::InvalidLaunchSpec(
            "settings overlay path must be absolute".into(),
        ));
    }
    match path.metadata() {
        Ok(meta) if meta.is_file() => Ok(path.to_path_buf()),
        Ok(_) => Err(DriverError::InvalidLaunchSpec(
            "settings overlay path is not a file".into(),
        )),
        Err(_) => Err(DriverError::InvalidLaunchSpec(
            "settings overlay path does not exist".into(),
        )),
    }
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
