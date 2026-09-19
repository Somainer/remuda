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
use remuda_protocol::{AgentKind, ClaudePermissionMode, PermissionMode};
use sha2::Digest as _;
use std::fs;
use std::path::Path;

/// The one capability name this batch knows (D-045).
pub const CAPABILITY_COMPUTER_USE: &str = "computer-use";

/// Child-env handshake the hardened `launch-cua-repl.sh` requires. A signal,
/// not a boundary: anything with a shell can `export` it, so the real boundary
/// is the absence of the materialized launcher when ungranted.
pub const CAPABILITY_COMPUTER_USE_ENV: &str = "REMUDA_CAPABILITY_COMPUTER_USE";

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

/// Relative paths of the launchers materialized under `<launch_dir>/cua`.
const LAUNCHER_REPL: &str = "scripts/launch-cua-repl.sh";

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
    /// Env name+value to push onto the recipe's env allowlist.
    pub env: (&'static str, &'static str),
}

/// Validate a requested capability list independently of any filesystem:
/// unknown values are refused (naming the value), and the origin/bypass/kind
/// gates are exactly the D-045 gates every carrier must enforce.
///
/// Returns true when `computer-use` is present. Callers that also need the
/// files written use [`materialize_grant`].
pub fn computer_use_requested(
    capabilities: &[String],
    origin: LaunchOrigin,
    permission: &PermissionMode,
    kind: AgentKind,
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
        // Q4: unattended desktop control plus skipped tool approvals is the
        // one combination with no recovery path. Both are named.
        if let PermissionMode::Claude(claude) = permission
            && claude.mode == ClaudePermissionMode::BypassPermissions
        {
            return Err(DriverError::InvalidLaunchSpec(format!(
                "refusing to grant {CAPABILITY_COMPUTER_USE:?} together with \
                 bypassPermissions on the same launch: unattended desktop control plus \
                 skipped tool approvals has no recovery path; remove one of the two"
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
    }
    Ok(capabilities
        .iter()
        .any(|value| value == CAPABILITY_COMPUTER_USE))
}

/// Materialize everything a granted launch is owed (D-045 §3.2/§3.3).
///
/// `native_home_managed` is the per-kind managed-home answer: true for claude
/// when the launch uses a Remuda-scoped config dir, false for an inherited
/// operator `~/.claude` (which is never written). It is ignored for codex,
/// whose only delivery leg this batch is the MCP config.
///
/// `shadow_via_session` is true for the shell-pty carrier, whose `HookSession`
/// materializes the codex shadow home (features + hook trust + MCP) after the
/// recipe; in that case this function must not write the codex `config.toml`
/// itself, or the second recipe materialization would erase the hook section.
/// Inputs for [`materialize_grant`], grouped to keep the grant's host facts
/// together (the same grouping the materializer already uses for overlays).
pub(crate) struct GrantRequest<'a> {
    /// Explicitly requested capabilities; empty means no grant.
    pub capabilities: &'a [String],
    /// Who originated the launch — agents may not self-grant.
    pub origin: LaunchOrigin,
    /// The launch's resolved permission mode (bypass gate).
    pub permission: &'a PermissionMode,
    pub kind: AgentKind,
    pub driver: remuda_protocol::DriverKind,
    pub launch_dir: &'a Path,
    pub native_home: &'a Path,
    /// Whether `native_home` is Remuda-managed (skill delivery allowed).
    pub native_home_managed: bool,
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
    if !computer_use_requested(capabilities, origin, permission, kind)? {
        return Ok(None);
    }

    let shadow_via_session = driver == remuda_protocol::DriverKind::ShellPty;
    let mut files = Vec::new();

    // Leg (a) for claude: the embedded skill tree into a *managed* home only.
    // codex/grok have no skills-directory reader, so writing there would be
    // delivery into a black hole (design §3.2); the inherited operator home is
    // never written on any branch.
    if kind == AgentKind::Claude && native_home_managed {
        write_skill_tree(native_home, &mut files)?;
    }

    // The launchers the MCP config points at, always under `<launch_dir>/cua`
    // so they exist with or without a managed skill tree (claude inherited
    // home, codex shadow home).
    let cua_dir = launch_dir.join("cua");
    let repl_rel = Path::new(LAUNCHER_REPL);
    let repl_launcher = cua_dir.join(repl_rel);
    let mcp_launcher = cua_dir.join("scripts/launch-mcp.sh");
    for (rel, dest, mode) in [
        (LAUNCHER_REPL, &repl_launcher, 0o755),
        ("scripts/launch-mcp.sh", &mcp_launcher, 0o755),
    ] {
        let bytes = embedded_bytes(rel)?;
        write_private_file(dest, bytes, mode)?;
        files.push(materialized_file(
            dest,
            FileRole::CapabilityScript,
            mode,
            bytes,
        )?);
    }

    // Leg (b): the one per-instance MCP config, 0600. It points at the
    // cua-repl launcher (the path for a host that is not an authenticated
    // Codex session — the Remuda case) and carries the env handshake.
    let config_path = launch_dir.join("mcp-cua.json");
    let config_json = serde_json::json!({
        "mcpServers": {
            MCP_SERVER_NAME: {
                "command": "/bin/sh",
                "args": [repl_launcher.to_string_lossy()],
                "env": { CAPABILITY_COMPUTER_USE_ENV: "1" },
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
    )?);

    let mcp_server = GrantedMcpServer {
        name: MCP_SERVER_NAME.to_owned(),
        config_path: config_path.to_string_lossy().into_owned(),
        command: "/bin/sh".to_owned(),
        args: vec![repl_launcher.to_string_lossy().into_owned()],
        env: vec![(CAPABILITY_COMPUTER_USE_ENV.to_owned(), "1".to_owned())],
    };

    // Codex reads `[mcp_servers.*]` in its shadow `config.toml`, never argv.
    // shell-pty gets the file from HookSession (hooks + this server); the
    // other codex carriers have no hook session, so write the MCP-only config
    // into their shadow home here.
    if kind == AgentKind::Codex && !shadow_via_session {
        let shadow_files = crate::launch::materialize_codex_mcp_servers(
            launch_dir,
            &[crate::launch::ShadowMcpServer {
                name: mcp_server.name.clone(),
                command: mcp_server.command.clone(),
                args: mcp_server.args.clone(),
                env: mcp_server.env.clone(),
            }],
        )?;
        for file in shadow_files {
            files.push(MaterializedFile {
                path: file.path.to_string_lossy().into_owned(),
                role: FileRole::CapabilityMcpConfig,
                mode: "0600".to_owned(),
                content_digest: file.digest,
                lifetime: FileLifetime::Launch,
            });
        }
    }

    // Mount by AgentKind, never by driver (design §3.3): codex takes the
    // shadow config.toml; everything else capable this batch (claude, however
    // hosted) takes argv.
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
        env: (CAPABILITY_COMPUTER_USE_ENV, "1"),
    }))
}

/// Write the embedded skill tree into `<native_home>/skills/codex-computer-use`.
/// Directories 0700, files 0600 (D-045).
fn write_skill_tree(native_home: &Path, files: &mut Vec<MaterializedFile>) -> DriverResult<()> {
    let root = native_home.join("skills").join(SKILL_DIR);
    for embedded in EMBEDDED_SKILL {
        let dest = root.join(embedded.rel);
        write_private_file(&dest, embedded.bytes, 0o600)?;
        files.push(materialized_file(
            &dest,
            FileRole::CapabilitySkill,
            0o600,
            embedded.bytes,
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
) -> DriverResult<MaterializedFile> {
    Ok(MaterializedFile {
        path: path.to_string_lossy().into_owned(),
        role,
        mode: format!("{mode:04o}"),
        content_digest: hash_bytes(bytes)?,
        lifetime: FileLifetime::Launch,
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
