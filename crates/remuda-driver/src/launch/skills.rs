//! Embedded `codex-computer-use` skill and per-launch capability delivery
//! (D-045, design `codex-cua.md` §3).
//!
//! Two rules shape everything here:
//!
//! * **Never materialize unless granted.** The capability is an explicit,
//!   per-launch string on [`remuda_protocol::InstanceSpec::capabilities`]; no
//!   default, preset or inheritance ever populates it. When it is absent this
//!   module writes nothing — and that, not the child-env handshake, is the
//!   security boundary: an ungranted agent has no `mcp-cua.json` and therefore
//!   no launcher path to point at.
//! * **Never write the operator's own home.** Skill bytes land only in a
//!   Remuda-managed home (the caller says which); every other file lives under
//!   this launch's `<launch_dir>`.
//!
//! The embedded copy is the one a musl Node with no checkout delivers, and it
//! must stay byte-identical to `skills/codex-computer-use/` — the digest test
//! in `tests/cua_capability.rs` fails when the two drift in bytes *or* in the
//! file set.

use crate::binary::hash_bytes;
use crate::error::{DriverError, DriverResult};
use crate::materializer::LaunchOrigin;
use crate::recipe::{FileLifetime, FileRole, GrantedMcpServer, MaterializedFile};
use remuda_protocol::{AgentKind, ApprovalPolicy, ClaudePermissionMode, PermissionMode};
use serde_json::Value;
use sha2::Digest as _;
use std::fs;
use std::path::{Path, PathBuf};

/// The one capability name this batch knows (D-045).
pub const CAPABILITY_COMPUTER_USE: &str = "computer-use";

/// Child-env handshake the hardened `launch-cua-repl.sh` requires. A signal,
/// not a boundary: anything with a shell can `export` it, so the real boundary
/// is the absence of the materialized launcher when ungranted.
pub const CAPABILITY_COMPUTER_USE_ENV: &str = "REMUDA_CAPABILITY_COMPUTER_USE";

/// Handshake value; only `1` means granted.
pub const CAPABILITY_COMPUTER_USE_VALUE: &str = "1";

/// MCP-server-only env carrying the **operator's real** codex home. A granted
/// codex launch shadows the agent's `CODEX_HOME` at a per-instance shadow home,
/// but the vendor app lives under the real home; the launchers resolve it from
/// this variable (skill scripts honor it first). Never set on the agent's own
/// environment — only on the MCP server entry — so agent state stays shadowed.
pub const CAPABILITY_REAL_CODEX_HOME_ENV: &str = "REMUDA_CODEX_HOME";

/// MCP server name used in both `mcp-cua.json` and codex `[mcp_servers.*]`.
pub const MCP_SERVER_NAME: &str = "codex-computer-use";

/// Embedded skill directory name (its frontmatter `name`).
pub const SKILL_DIR: &str = "codex-computer-use";

struct EmbeddedFile {
    /// Path relative to the skill root.
    rel: &'static str,
    bytes: &'static [u8],
}

/// The whole shipped skill, embedded in the binary. Every file under
/// `skills/codex-computer-use/` must be listed here; the digest-equality test
/// fails when the on-disk set and this list disagree.
const EMBEDDED_SKILL: &[EmbeddedFile] = &[
    EmbeddedFile {
        rel: "SKILL.md",
        bytes: include_str!("../../../../skills/codex-computer-use/SKILL.md").as_bytes(),
    },
    EmbeddedFile {
        rel: "references/setup.md",
        bytes: include_str!("../../../../skills/codex-computer-use/references/setup.md").as_bytes(),
    },
    EmbeddedFile {
        rel: "scripts/launch-cua-repl.sh",
        bytes: include_str!("../../../../skills/codex-computer-use/scripts/launch-cua-repl.sh")
            .as_bytes(),
    },
    EmbeddedFile {
        rel: "scripts/launch-mcp.sh",
        bytes: include_str!("../../../../skills/codex-computer-use/scripts/launch-mcp.sh")
            .as_bytes(),
    },
    EmbeddedFile {
        rel: "scripts/probe_mcp.py",
        bytes: include_str!("../../../../skills/codex-computer-use/scripts/probe_mcp.py")
            .as_bytes(),
    },
];

/// Relative path of the cua-repl launcher materialized under `<launch_dir>/cua`.
const LAUNCHER_REPL: &str = "scripts/launch-cua-repl.sh";
/// Relative path of the native-MCP launcher.
const LAUNCHER_MCP: &str = "scripts/launch-mcp.sh";

/// Everything a granted launch receives; computed and written together so the
/// argv, env and audit record can never describe a file that does not exist.
pub(crate) struct CapabilityGrant {
    /// Capability names carried onto the recipe (audit).
    pub capabilities: Vec<String>,
    /// 0600/0755 files written for this launch, for `materialized_files`.
    pub files: Vec<MaterializedFile>,
    /// The registered MCP server, for argv (claude) / shadow config (codex).
    pub mcp_server: GrantedMcpServer,
    /// Argv tokens to append (claude only; empty for codex).
    pub argv: Vec<String>,
    /// Env name+value to push onto the recipe's env allowlist. The name is the
    /// single allowlisted REMUDA_ variable; the value rides with the grant,
    /// never hardcoded at the spawn sites.
    pub env: (&'static str, &'static str),
}

/// Validate a requested capability list independently of any filesystem:
/// unknown values are refused (naming the value), and the origin/bypass/kind/
/// carrier gates are exactly the D-045 gates every carrier must enforce.
///
/// `driver` is required because codex delivery exists only on shell-pty; the
/// carrier gate runs here (before the materializer writes the launch dir or
/// any overlay) so a refusal never leaves capability files behind.
///
/// Returns true when `computer-use` is present. Callers that also need the
/// files written use [`materialize_grant`].
pub fn computer_use_requested(
    capabilities: &[String],
    origin: LaunchOrigin,
    permission: &PermissionMode,
    kind: AgentKind,
    driver: remuda_protocol::DriverKind,
) -> DriverResult<bool> {
    for value in capabilities {
        if value != CAPABILITY_COMPUTER_USE {
            return Err(DriverError::InvalidLaunchSpec(format!(
                "unknown launch capability {value:?}; this build accepts only \
                 \"{CAPABILITY_COMPUTER_USE}\""
            )));
        }
    }
    if !capabilities.is_empty() {
        // Gate 1 (D-045 §2): an agent never mints desktop control for itself.
        if matches!(origin, LaunchOrigin::Agent) {
            return Err(DriverError::InvalidLaunchSpec(format!(
                "an agent-originated launch may not grant the \
                 {CAPABILITY_COMPUTER_USE:?} capability (origin={origin:?}); only an \
                 explicit human or bot launch may request it"
            )));
        }
        // Q4, harness-agnostic: unattended/auto-approved desktop control is the
        // one combination with no recovery path. Claude's spelling is
        // bypassPermissions; codex's is approval policy `never` (auto-approve
        // every action). Both are named in the refusal.
        let unattended = match permission {
            PermissionMode::Claude(claude) => {
                claude.mode == ClaudePermissionMode::BypassPermissions
            }
            PermissionMode::Codex(codex) => codex.approval_policy == ApprovalPolicy::Never,
            _ => false,
        };
        if unattended {
            return Err(DriverError::InvalidLaunchSpec(format!(
                "refusing to grant {CAPABILITY_COMPUTER_USE:?} together with \
                 unattended/skipped tool approvals on the same {kind:?} launch: desktop \
                 control plus auto-approved actions has no recovery path; remove one of the two"
            )));
        }
        // grok (and agy/generic/terminal) are not capability targets this batch.
        match kind {
            AgentKind::Claude | AgentKind::Codex => {}
            other => {
                return Err(DriverError::InvalidLaunchSpec(format!(
                    "the {CAPABILITY_COMPUTER_USE:?} capability is not supported for \
                     agent kind {other:?} this batch; supported kinds are claude and codex"
                )));
            }
        }
        // Codex delivery exists only on shell-pty (HookSession splices the
        // granted server into a complete shadow CODEX_HOME). Refuse before any
        // file is written; the Node factory additionally requires hooks on.
        if kind == AgentKind::Codex && driver != remuda_protocol::DriverKind::ShellPty {
            return Err(DriverError::InvalidLaunchSpec(format!(
                "the \"computer-use\" capability for codex on {driver:?} is not delivered: \
                 only shell-pty with REMUDA_PTY_HOOKS=1 materializes the shadow CODEX_HOME \
                 the granted MCP server needs; the other carriers would shadow the operator's \
                 codex login"
            )));
        }
    }
    Ok(capabilities
        .iter()
        .any(|value| value == CAPABILITY_COMPUTER_USE))
}

/// Refuse a caller-supplied `--mcp-config` that would collide with the granted
/// server. Runs before any file is written; accepts both `--mcp-config path`
/// and `--mcp-config=path` spellings (D-045 leg (b)).
pub fn reject_caller_mcp_config(capabilities: &[String], args: &[String]) -> DriverResult<()> {
    if !capabilities
        .iter()
        .any(|value| value == CAPABILITY_COMPUTER_USE)
    {
        return Ok(());
    }
    let collides = args
        .iter()
        .any(|arg| arg == "--mcp-config" || arg.starts_with("--mcp-config="));
    if collides {
        return Err(DriverError::InvalidLaunchSpec(
            "--mcp-config is supplied by Remuda for the granted computer-use capability; \
             a caller-supplied --mcp-config (including --mcp-config=<path>) collides with it"
                .into(),
        ));
    }
    Ok(())
}

/// Inputs for [`materialize_grant`], grouped to keep the grant's host facts
/// together (the same grouping the materializer already uses for overlays).
pub(crate) struct GrantRequest<'a> {
    /// Explicitly requested capabilities; empty means no grant.
    pub capabilities: &'a [String],
    /// Who originated the launch — agents may not self-grant.
    pub origin: LaunchOrigin,
    /// The launch's resolved permission mode (unattended gate).
    pub permission: &'a PermissionMode,
    pub kind: AgentKind,
    /// Carrier driver. Codex delivery is shell-pty + HookSession only; any
    /// other driver makes the grant undeliverable at this layer and is
    /// refused (the Node factory enforces the hooks-on half of the gate).
    pub driver: remuda_protocol::DriverKind,
    pub launch_dir: &'a Path,
    pub native_home: &'a Path,
    /// Whether `native_home` is Remuda-managed (skill delivery allowed).
    pub native_home_managed: bool,
}

/// Whether a managed home is shared across instances (so its skill files must
/// not be shredded on one instance's exit).
///
/// The Node roots a per-instance home at `<instance_dir>/native-home` (and the
/// codex shadow at `<instance_dir>/launch/codex-home`); a configured shared
/// dir (`REMUDA_CLAUDE_CONFIG_DIR`) or an explicit operator dir lives outside
/// the instance dir. Path containment is the exact distinction the Node's home
/// construction gives, so the materializer need not carry another flag.
pub(crate) fn native_home_is_shared(launch_dir: &Path, native_home: &Path) -> bool {
    let instance_dir = launch_dir.parent();
    instance_dir
        .map(|instance_dir| !native_home.starts_with(instance_dir))
        .unwrap_or(false)
}

pub(crate) fn materialize_grant(
    request: &GrantRequest<'_>,
) -> DriverResult<Option<CapabilityGrant>> {
    let GrantRequest {
        capabilities,
        origin,
        permission,
        kind,
        driver,
        launch_dir,
        native_home,
        native_home_managed,
    } = *request;
    if !computer_use_requested(capabilities, origin, permission, kind, driver)? {
        return Ok(None);
    }

    let mut files = Vec::new();

    // Leg (a) for claude: the embedded skill tree into a *managed* home only.
    // codex/grok have no skills-directory reader, so writing there would be
    // delivery into a black hole (design §3.2); the inherited operator home is
    // never written on any branch.
    //
    // Lifetime follows containment: files in a home outside this instance dir
    // (configured shared dir / explicit operator dir) survive the launch and
    // are not shredded; per-instance homes are cleaned up with it.
    let skill_lifetime = if native_home_is_shared(launch_dir, native_home) {
        FileLifetime::NativeStore
    } else {
        FileLifetime::Launch
    };
    if kind == AgentKind::Claude && native_home_managed {
        write_skill_tree(native_home, skill_lifetime, &mut files)?;
    }

    // The launchers the MCP config points at, always under `<launch_dir>/cua`
    // so they exist with or without a managed skill tree (claude inherited
    // home, codex shadow home).
    let cua_dir = launch_dir.join("cua");
    let repl_launcher = cua_dir.join(LAUNCHER_REPL);
    let mcp_launcher = cua_dir.join(LAUNCHER_MCP);
    for (rel, dest) in [
        (LAUNCHER_REPL, &repl_launcher),
        (LAUNCHER_MCP, &mcp_launcher),
    ] {
        let bytes = embedded_bytes(rel)?;
        write_private_file(dest, bytes, 0o755)?;
        files.push(materialized_file(
            dest,
            FileRole::CapabilityScript,
            0o755,
            bytes,
            FileLifetime::Launch,
        )?);
    }

    // The launchers locate the vendor app under the *operator's real* codex
    // home; the agent itself keeps the shadowed CODEX_HOME, so the real home is
    // handed to the launcher process only, via the MCP server entry env.
    let real_codex_home = resolve_real_codex_home();

    // Leg (b): the one per-instance MCP config, 0600. It points at the
    // cua-repl launcher (the path for a host that is not an authenticated
    // Codex session — the Remuda case) and carries the handshake plus the
    // launcher-only real codex home.
    let mut server_env = vec![(
        CAPABILITY_COMPUTER_USE_ENV.to_owned(),
        CAPABILITY_COMPUTER_USE_VALUE.to_owned(),
    )];
    if let Some(real) = &real_codex_home {
        server_env.push((CAPABILITY_REAL_CODEX_HOME_ENV.to_owned(), real.clone()));
    }

    let config_path = launch_dir.join("mcp-cua.json");
    let mut server_env_map = serde_json::Map::new();
    for (key, value) in &server_env {
        server_env_map.insert(key.clone(), Value::String(value.clone()));
    }
    let config_json = serde_json::json!({
        "mcpServers": {
            MCP_SERVER_NAME: {
                "command": "/bin/sh",
                "args": [repl_launcher.to_string_lossy()],
                "env": server_env_map,
            }
        }
    });
    let config_bytes = serde_json::to_vec_pretty(&config_json)?;
    write_private_file(&config_path, &config_bytes, 0o600)?;
    files.push(materialized_file(
        &config_path,
        FileRole::CapabilityMcpConfig,
        0o600,
        &config_bytes,
        FileLifetime::Launch,
    )?);

    let mcp_server = GrantedMcpServer {
        name: MCP_SERVER_NAME.to_owned(),
        config_path: config_path.to_string_lossy().into_owned(),
        command: "/bin/sh".to_owned(),
        args: vec![repl_launcher.to_string_lossy().into_owned()],
        env: server_env,
    };

    // Codex reads `[mcp_servers.*]` in its shadow `config.toml`, never argv.
    // That shadow home is materialized by the shell-pty HookSession (the only
    // codex carrier the Node gate permits for a grant); the session splices
    // `mcp_server` into the complete features+trust+MCP config. The materializer
    // itself writes no codex config.toml — a partial home would shadow the
    // operator's login.

    // Mount by AgentKind, never by driver (design §3.3): codex takes the
    // shadow config.toml; claude (however hosted) takes argv.
    let argv = if kind == AgentKind::Claude {
        vec![
            "--mcp-config".to_owned(),
            config_path.to_string_lossy().into_owned(),
        ]
    } else {
        Vec::new()
    };

    Ok(Some(CapabilityGrant {
        capabilities: vec![CAPABILITY_COMPUTER_USE.to_owned()],
        files,
        mcp_server,
        argv,
        env: (CAPABILITY_COMPUTER_USE_ENV, CAPABILITY_COMPUTER_USE_VALUE),
    }))
}

/// Resolve the operator's real codex home the way the launchers do
/// (`CODEX_HOME`, else `$HOME/.codex`). Runs in the Node process before any
/// per-child shadow env is applied, so it never sees the shadow value.
fn resolve_real_codex_home() -> Option<String> {
    if let Some(path) = std::env::var_os(CAPABILITY_REAL_CODEX_HOME_ENV)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
    {
        // An explicit test/operator override wins.
        return Some(path.to_string_lossy().into_owned());
    }
    if let Some(path) = std::env::var_os("CODEX_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
    {
        return Some(path.to_string_lossy().into_owned());
    }
    std::env::var_os("HOME")
        .filter(|path| !path.is_empty())
        .map(|home| {
            PathBuf::from(home)
                .join(".codex")
                .to_string_lossy()
                .into_owned()
        })
}

/// Write the embedded skill tree into `<native_home>/skills/codex-computer-use`.
/// Every directory in the created tree and every file are owner-only
/// (0700 dirs, 0600 files; D-045). File lifetime follows the home: per-instance
/// files are shredded with the launch; shared-home files survive it.
fn write_skill_tree(
    native_home: &Path,
    lifetime: FileLifetime,
    files: &mut Vec<MaterializedFile>,
) -> DriverResult<()> {
    let skills_root = native_home.join("skills");
    let root = skills_root.join(SKILL_DIR);

    // Create + 0700 every directory component the files occupy, starting at
    // `native_home/skills` (create_dir_all alone leaves intermediate dirs at
    // umask, including the skills root itself).
    let mut dirs: Vec<PathBuf> = EMBEDDED_SKILL
        .iter()
        .filter_map(|file| {
            Path::new(file.rel)
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .map(|parent| root.join(parent))
        })
        .collect();
    dirs.sort();
    dirs.dedup();
    fs::create_dir_all(&root)?;
    set_dir_mode(&skills_root, 0o700)?;
    set_dir_mode(&root, 0o700)?;
    for dir in &dirs {
        fs::create_dir_all(dir)?;
        set_dir_mode(dir, 0o700)?;
    }

    for embedded in EMBEDDED_SKILL {
        let dest = root.join(embedded.rel);
        write_private_file(&dest, embedded.bytes, 0o600)?;
        files.push(materialized_file(
            &dest,
            FileRole::CapabilitySkill,
            0o600,
            embedded.bytes,
            lifetime,
        )?);
    }
    Ok(())
}

fn embedded_bytes(rel: &str) -> DriverResult<&'static [u8]> {
    EMBEDDED_SKILL
        .iter()
        .find(|file| file.rel == rel)
        .map(|file| file.bytes)
        .ok_or_else(|| {
            DriverError::SettingsIsolationUnavailable(format!("embedded skill is missing {rel}"))
        })
}

fn materialized_file(
    path: &Path,
    role: FileRole,
    mode: u32,
    bytes: &[u8],
    lifetime: FileLifetime,
) -> DriverResult<MaterializedFile> {
    Ok(MaterializedFile {
        path: path.to_string_lossy().into_owned(),
        role,
        mode: format!("{mode:04o}"),
        content_digest: hash_bytes(bytes)?,
        lifetime,
    })
}

/// SHA-256 digest over a sorted `(path, bytes)` list; the embedded copy and the
/// on-disk skill directory are compared through this in the drift test.
pub fn skill_tree_digest<'a, I>(entries: I) -> String
where
    I: IntoIterator<Item = (&'a str, &'a [u8])>,
{
    let mut entries: Vec<(&str, &[u8])> = entries.into_iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    let mut hasher = sha2::Sha256::new();
    for (rel, bytes) in entries {
        hasher.update(rel.as_bytes());
        hasher.update([0u8]);
        hasher.update(bytes);
        hasher.update([0u8]);
    }
    format!("{:x}", hasher.finalize())
}

fn write_private_file(path: &Path, contents: &[u8], mode: u32) -> DriverResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        set_dir_mode(parent, 0o700)?;
    }
    fs::write(path, contents)?;
    set_file_mode(path, mode)?;
    Ok(())
}

#[cfg(unix)]
fn set_file_mode(path: &Path, mode: u32) -> DriverResult<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_file_mode(path: &Path, _mode: u32) -> DriverResult<()> {
    let _ = path;
    Ok(())
}

#[cfg(unix)]
fn set_dir_mode(path: &Path, mode: u32) -> DriverResult<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_dir_mode(path: &Path, _mode: u32) -> DriverResult<()> {
    let _ = path;
    Ok(())
}

/// Iterate the embedded file set (path relative to the skill root, bytes).
pub fn embedded_skill_files() -> impl Iterator<Item = (&'static str, &'static [u8])> {
    EMBEDDED_SKILL.iter().map(|file| (file.rel, file.bytes))
}
