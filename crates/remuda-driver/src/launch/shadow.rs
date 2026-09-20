//! Shadow `CODEX_HOME` / `GROK_HOME` trees (D-028 §4.2, §5.1, P6).
//!
//! The Claude overlay reaches the harness through a `--settings` merge; codex
//! and grok instead read a whole home directory. These functions materialize a
//! per-session shadow home under `<instance dir>/launch/` that points the
//! harness at the instance hook socket and nothing else. The user's own
//! `~/.codex` / `~/.grok` is never written; the child receives the shadow
//! path through the driver-computed environment (`CODEX_HOME` /
//! `GROK_HOME`), which is exactly the §4.2 boundary.
//!
//! Two harness-specific rules live here:
//!
//! * **codex hook trust.** The client refuses a hook command until a
//!   per-handler canonical hash is persisted. The placement, key and hash
//!   were all measured against a live codex app-server on this host
//!   (0.147), and the recorded hashes match 0.154 [codex-signals-1] §A1:
//!   - trust lives under `[hooks.state]` **inside `config.toml`** — the
//!     app-server's `config/batchWrite` uses `keyPath:"hooks.state"`,
//!     `filePath:"config.toml"`. A standalone `hooks.state` file is not
//!     loaded (empirically: `trustStatus` stays `untrusted`);
//!   - the trust key is `<absolute hooks.json>:<snake_event>:0:0`;
//!   - the hash is `sha256:` of compact, recursively-key-sorted
//!     `{event_name, hooks:[{async:false, command, timeout, type}]}` — the
//!     handler `type` tag participates, the matcher is normalized away.
//! * **grok cross-reads `~/.claude`.** Even with `GROK_HOME` redirected,
//!   grok merges global Claude hooks (§3.1 [V]), which `CLAUDE_CONFIG_DIR`
//!   cannot stop. The neutraliser is `GROK_CLAUDE_HOOKS_ENABLED=0`, part of
//!   the returned environment.
//!
//! [codex-signals-1]: ../../../../docs/design/evidence/codex-signals-1.md

use crate::binary::hash_bytes;
use crate::error::{DriverError, DriverResult};
use remuda_protocol::{AgentKind, Digest};
use serde_json::{Value, json};
use sha2::Digest as _;
use std::path::{Path, PathBuf};

/// (PascalCase event, snake_case hash-key event) for codex.
const CODEX_EVENTS: &[(&str, &str)] = &[
    ("SessionStart", "session_start"),
    // The blocking approval channel (§4.4). Codex normalises the tool name to
    // `Bash`; the stdin payload carries no `tool_use_id` (evidence A1).
    ("PermissionRequest", "permission_request"),
];

/// Hook events grok loads. `PermissionRequest` is deliberately absent: grok's
/// loader silently ignores that name (grok-signals-1 §A3).
const GROK_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "Notification",
    "Stop",
];

/// Ordinary command hooks default to 600 seconds; the canonical hash includes
/// the timeout, so the written timeout and the hashed timeout must agree
/// (evidence §A1).
const CODEX_HOOK_TIMEOUT_SECS: u64 = 600;

/// One materialized shadow home and how to reach it.
pub struct ShadowHome {
    /// Which harness this is.
    pub kind: AgentKind,
    /// Root of the shadow home (value of `CODEX_HOME` / `GROK_HOME`).
    pub home: PathBuf,
    /// Environment the child must receive, including the home variable itself
    /// (`CODEX_HOME` / `GROK_HOME`) and, for grok, the Claude-hooks switch.
    pub env: Vec<(String, String)>,
    /// 0600 files written, for the launch audit.
    pub files: Vec<ShadowFile>,
}

/// A file written inside a shadow home.
#[derive(Debug, Clone)]
pub struct ShadowFile {
    /// Absolute path.
    pub path: PathBuf,
    /// Why it exists.
    pub role: &'static str,
    /// SHA-256 of the written bytes.
    pub digest: Digest,
}

/// One MCP server spliced into the codex shadow `config.toml` (D-045 §3.3).
#[derive(Debug, Clone)]
pub struct ShadowMcpServer {
    /// Table name under `[mcp_servers.<name>]`.
    pub name: String,
    /// Executable the app-server spawns for the stdio server.
    pub command: String,
    /// Executable arguments.
    pub args: Vec<String>,
    /// Server-process environment (the capability handshake lives here).
    pub env: Vec<(String, String)>,
}

/// Inputs to shadow-home materialisation.
pub struct ShadowOptions<'a> {
    /// `<instance dir>/launch`.
    pub launch_dir: &'a Path,
    /// The `remuda` binary the relay runs as.
    pub relay_binary: &'a Path,
    /// The real path the hook socket is bound at. Conventionally
    /// `<instance dir>/hook.sock`; under a long data dir it is the short
    /// per-user runtime path (the under-instance name is only a symlink,
    /// which connect(2) cannot traverse once it exceeds `sun_path`).
    pub socket_path: &'a Path,
    /// MCP servers appended to the codex `config.toml` (D-045). Grok rejects
    /// the capability at the materializer, so these are codex-only in practice.
    pub mcp_servers: &'a [ShadowMcpServer],
}

/// Materialize a shadow home for `kind`; only codex and grok have one.
pub fn materialize(kind: AgentKind, options: &ShadowOptions<'_>) -> DriverResult<ShadowHome> {
    match kind {
        AgentKind::Codex => materialize_codex(options),
        AgentKind::Grok => materialize_grok(options),
        _ => Err(DriverError::SettingsIsolationUnavailable(
            "only codex and grok have shadow homes".into(),
        )),
    }
}

/// Materialize the codex shadow home: `config.toml` (features + hook trust)
/// and `hooks.json` (the relay for SessionStart and PermissionRequest).
pub fn materialize_codex(options: &ShadowOptions<'_>) -> DriverResult<ShadowHome> {
    let home = options.launch_dir.join("codex-home");
    std::fs::create_dir_all(&home)?;
    set_dir_mode(&home, 0o700)?;

    let hooks_path = home.join("hooks.json");
    let mut hooks = serde_json::Map::new();
    let mut trust = String::new();
    for (pascal, snake) in CODEX_EVENTS {
        let command = relay_for(options.relay_binary, options.socket_path, pascal);
        hooks.insert(
            (*pascal).to_owned(),
            json!([{
                "hooks": [{
                    "type": "command",
                    "command": command,
                    "timeout": CODEX_HOOK_TIMEOUT_SECS,
                }]
            }]),
        );
        let hash = codex_trust_hash(snake, &command, CODEX_HOOK_TIMEOUT_SECS);
        // `<absolute hooks.json>:<snake event>:<matcher ord>:<handler ord>`
        // (evidence §A1: `/…/hooks.json:permission_request:0:0`).
        let key = format!("{}:{snake}:0:0", hooks_path.to_string_lossy());
        trust.push_str("[hooks.state.");
        trust.push_str(&toml_basic_string(&key));
        trust.push_str("]\ntrusted_hash = ");
        trust.push_str(&toml_basic_string(&hash));
        trust.push_str("\n\n");
    }
    write_private(
        &hooks_path,
        &serde_json::to_vec_pretty(&json!({ "hooks": hooks }))?,
        0o600,
    )?;

    // Features and trust state in ONE config.toml. The app-server reads
    // hooks.state from config.toml; a standalone hooks.state file is ignored
    // (verified against the live app-server).
    let config_path = home.join("config.toml");
    // D-045 leg (b): granted MCP servers as `[mcp_servers.*]`, appended
    // without disturbing `[features]` or `[hooks.state.*]`.
    let mcp_section = render_mcp_servers(options.mcp_servers);
    let config = format!(
        "# Generated by Remuda for one session (D-028 P6).\n\
         [features]\n\
         hooks = true\n\n\
         {mcp_section}\
         {trust}"
    );
    write_private(&config_path, config.as_bytes(), 0o600)?;

    let files = vec![
        shadow_file(&config_path, "codex-config")?,
        shadow_file(&hooks_path, "codex-hooks")?,
    ];
    let home_value = home.to_string_lossy().into_owned();
    Ok(ShadowHome {
        kind: AgentKind::Codex,
        home,
        env: vec![("CODEX_HOME".to_owned(), home_value)],
        files,
    })
}

/// Materialize the grok shadow home: one merged `hooks/*.json` pointing at the
/// relay, plus the switch that neutralizes grok's cross-read of claude hooks.
pub fn materialize_grok(options: &ShadowOptions<'_>) -> DriverResult<ShadowHome> {
    let home = options.launch_dir.join("grok-home");
    let hooks_dir = home.join("hooks");
    std::fs::create_dir_all(&hooks_dir)?;
    set_dir_mode(&home, 0o700)?;
    set_dir_mode(&hooks_dir, 0o700)?;

    let mut hooks = serde_json::Map::new();
    for event in GROK_EVENTS {
        let command = relay_for(options.relay_binary, options.socket_path, event);
        hooks.insert(
            (*event).to_owned(),
            json!([{
                "hooks": [{
                    "type": "command",
                    "command": command,
                    "timeout": CODEX_HOOK_TIMEOUT_SECS,
                }]
            }]),
        );
    }
    let hooks_path = hooks_dir.join("remuda.json");
    write_private(
        &hooks_path,
        &serde_json::to_vec_pretty(&json!({ "hooks": hooks }))?,
        0o600,
    )?;
    let config_path = home.join("config.toml");
    write_private(
        &config_path,
        b"# Generated by Remuda for one session (D-028 P6).\n",
        0o600,
    )?;

    let files = vec![
        shadow_file(&config_path, "grok-config")?,
        shadow_file(&hooks_path, "grok-hooks")?,
    ];
    // §3.1 [V]: the only switch that stops grok cross-running the user's
    // global Claude hooks. Computed before `home` moves into the struct.
    let home_value = home.to_string_lossy().into_owned();
    Ok(ShadowHome {
        kind: AgentKind::Grok,
        home,
        env: vec![
            ("GROK_HOME".to_owned(), home_value),
            (GROK_CLAUDE_HOOKS_ENV.to_owned(), "0".to_owned()),
        ],
        files,
    })
}

/// Env switch that disables grok's read of the user's claude hooks.
pub const GROK_CLAUDE_HOOKS_ENV: &str = "GROK_CLAUDE_HOOKS_ENABLED";

/// Build a relay command for one event.
fn relay_for(relay_binary: &Path, socket_path: &Path, event: &str) -> String {
    format!(
        "{} hook emit --socket {} --event {}",
        shell_quote(&relay_binary.to_string_lossy()),
        shell_quote(&socket_path.to_string_lossy()),
        shell_quote(event),
    )
}

/// Compute the codex per-handler trust hash.
///
/// Canonical identity (verified against the live app-server, and matching the
/// hashes recorded for 0.154 in evidence §A1):
///
/// ```json
/// {"event_name":"<snake>","hooks":[{"async":false,"command":"<cmd>",
///   "timeout":<secs>,"type":"command"}]}
/// ```
///
/// Object keys are recursively sorted; JSON is compact. The handler `type`
/// tag participates; the matcher is normalized away by codex.
#[must_use]
pub fn codex_trust_hash(event_snake: &str, command: &str, timeout_secs: u64) -> String {
    let identity = json!({
        "event_name": event_snake,
        "hooks": [{
            "async": false,
            "command": command,
            "timeout": timeout_secs,
            "type": "command",
        }],
    });
    let canonical = canonical_json(&identity);
    let digest = sha2::Sha256::digest(canonical.as_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("sha256:{hex}")
}

/// Render granted MCP servers as codex `config.toml` tables.
///
/// Codex parses `[mcp_servers.<name>]` with `command` / `args` and an optional
/// `[mcp_servers.<name>.env]` table (verified shape: same stdio contract as the
/// CLI's own MCP config). Server names are quoted: capability names contain
/// hyphens, which are not legal in every TOML bare-key position.
fn render_mcp_servers(servers: &[ShadowMcpServer]) -> String {
    let mut out = String::new();
    for server in servers {
        out.push_str("[mcp_servers.");
        out.push_str(&toml_basic_string(&server.name));
        out.push_str("]\ncommand = ");
        out.push_str(&toml_basic_string(&server.command));
        out.push('\n');
        if !server.args.is_empty() {
            out.push_str("args = [");
            out.push_str(
                &server
                    .args
                    .iter()
                    .map(|arg| toml_basic_string(arg))
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            out.push_str("]\n");
        }
        if !server.env.is_empty() {
            out.push_str("[mcp_servers.");
            out.push_str(&toml_basic_string(&server.name));
            out.push_str(".env]\n");
            for (key, value) in &server.env {
                out.push_str(&toml_basic_string(key));
                out.push_str(" = ");
                out.push_str(&toml_basic_string(value));
                out.push('\n');
            }
        }
        out.push('\n');
    }
    out
}

/// Recursively key-sorted, compact JSON (no spaces), matching codex's
/// fingerprint canonicalisation.
fn canonical_json(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|(a, _), (b, _)| a.cmp(b));
            let body = entries
                .iter()
                .map(|(key, value)| format!("{}:{}", json_string(key), canonical_json(value)))
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{body}}}")
        }
        Value::Array(items) => {
            let body = items
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",");
            format!("[{body}]")
        }
        scalar => serde_json::to_string(scalar).expect("scalar json"),
    }
}

/// Compact JSON string escaping — the same TOML basic strings accept for the
/// characters these keys/hashes contain (no raw controls).
fn json_string(value: &str) -> String {
    serde_json::to_string(value).expect("string json")
}

/// Single-quote a value for a POSIX `sh -c` command.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// TOML basic string literal. These values are paths, hashes and keys (all
/// ASCII in practice); JSON escaping is a superset of TOML basic-string
/// escaping for that alphabet.
fn toml_basic_string(value: &str) -> String {
    serde_json::to_string(value).expect("string json")
}

fn write_private(path: &Path, contents: &[u8], mode: u32) -> DriverResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents)?;
    set_file_mode(path, mode)?;
    Ok(())
}

fn shadow_file(path: &Path, role: &'static str) -> DriverResult<ShadowFile> {
    let bytes = std::fs::read(path)?;
    Ok(ShadowFile {
        path: path.to_path_buf(),
        role,
        digest: hash_bytes(&bytes)?,
    })
}

#[cfg(unix)]
fn set_file_mode(path: &Path, mode: u32) -> DriverResult<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(unix)]
fn set_dir_mode(path: &Path, mode: u32) -> DriverResult<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_file_mode(_path: &Path, _mode: u32) -> DriverResult<()> {
    Ok(())
}

#[cfg(not(unix))]
fn set_dir_mode(_path: &Path, _mode: u32) -> DriverResult<()> {
    Ok(())
}

#[cfg(test)]
mod tests;
