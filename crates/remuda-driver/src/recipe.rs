//! Persistent launch recipe. Serializes without secret values or prompts.

use remuda_protocol::{ApprovalAuthority, BoolLiteral, Digest, DriverKind, Id, InputDelivery};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;

/// M0 technical-debt tag applied when `--permission-mode dontAsk` is emitted.
pub const TECH_DEBT_M0_PERM_01: &str = "TD-M0-PERM-01";

/// How an allowlisted env name will be supplied at spawn time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EnvAllowlistSource {
    /// Literal from the spec; value is not stored on the recipe.
    Literal,
    /// Credential/secret broker reference.
    Credential,
    /// Inherited from the Node host environment by name.
    HostEnv,
    /// Registered native home path (`CLAUDE_CONFIG_DIR` / `CODEX_HOME` / …).
    NativeHome,
    /// Non-secret provider overlay (base URL, discovery flags).
    ProviderOverlay,
    /// A value Remuda itself attaches because a per-launch host capability
    /// (`computer-use`; D-045) was granted. The value is driver-computed, never
    /// read from the spec or the Node's environment — the `REMUDA_` deny
    /// prefix applies to both of those sources, but not to this handshake.
    Capability,
}

/// One env name that spawn may inject. Values never appear here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvAllowlistEntry {
    /// Environment variable name.
    pub name: String,
    /// How the value will be obtained at spawn.
    pub source: EnvAllowlistSource,
    /// Secret or credential reference, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_ref: Option<String>,
}

/// Role of a file the materializer wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FileRole {
    /// Claude `--settings` JSON overlay.
    Settings,
    /// Claude `apiKeyHelper` script written next to the settings overlay.
    ApiKeyHelper,
    /// Codex/Grok config overlay.
    ProviderConfig,
    /// Per-instance `mcp-cua.json` naming the granted MCP server (D-045).
    CapabilityMcpConfig,
    /// Launcher script written from the embedded skill bytes.
    CapabilityScript,
    /// One file of the embedded skill tree materialized into a managed home.
    CapabilitySkill,
}

/// One MCP server a capability grant registered for this launch (D-045 §3.3).
///
/// Claude-shaped carriers receive the same server through the per-instance
/// `mcp-cua.json` on argv; codex receives it as `[mcp_servers.<name>]` in its
/// shadow `config.toml`. Values are non-secret launch facts, safe to audit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GrantedMcpServer {
    /// MCP server name (`codex-computer-use`).
    pub name: String,
    /// Absolute path of the per-instance MCP config that names it.
    pub config_path: String,
    /// Executable the host runs for the stdio server.
    pub command: String,
    /// Executable arguments; the launcher script is the last entry.
    pub args: Vec<String>,
    /// Environment the server process receives (the capability handshake).
    pub env: Vec<(String, String)>,
}

/// How long a materialized file must be retained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FileLifetime {
    /// Deleted after the child exits.
    Launch,
    /// Lives with the registered native store.
    NativeStore,
}

/// File written at 0600 for this launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MaterializedFile {
    /// Absolute path.
    pub path: String,
    /// Why the file exists.
    pub role: FileRole,
    /// `0600` for overlays; `0700` for [`FileRole::ApiKeyHelper`].
    pub mode: String,
    /// SHA-256 of the file bytes.
    pub content_digest: Digest,
    /// Retention.
    pub lifetime: FileLifetime,
}

/// Audit block persisted with the recipe. No secret values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchAudit {
    /// Every env name that will be injected, inherited, or stripped.
    pub env_names: Vec<String>,
    /// Credential/secret references (not values).
    pub credential_refs: Vec<String>,
    /// Argv with settings paths replaced by `<settings>`.
    pub redacted_argv: Vec<String>,
    /// Digest of the settings overlay, when one was written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings_digest: Option<Digest>,
    /// Always true when a recipe exists: banned flags were checked.
    pub prohibited_options_checked: BoolLiteral<true>,
    /// Whether `spec.binaryPath` replaced the driver's default executable.
    ///
    /// The pin itself is already on [`LaunchRecipe::binary`]; this records that
    /// the choice was the caller's, so an audit can tell a stock launch from
    /// one that ran an operator-nominated binary without diffing paths.
    ///
    /// `#[serde(default)]` keeps recipes persisted before this field existed
    /// readable — an old record predates the feature, so `false` is correct.
    #[serde(default)]
    pub binary_override: bool,
    /// Who will answer permission prompts for this launch.
    pub approval_authority: ApprovalAuthority,
}

/// Provider fields copied into the recipe (no secrets).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecipeProvider {
    /// Profile id.
    pub profile_id: Id,
    /// Wire kind.
    pub kind: crate::profile::ProviderKind,
    /// Ingress URL.
    pub base_url: String,
    /// Delegation mode (`none` / `gateway` / `direct`).
    pub delegation: crate::profile::Delegation,
    /// Secret reference spelling (scheme + name/path), never the secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_ref: Option<String>,
    /// Model id the launch was *asked* to use. This is `spec.model_id` when one
    /// was explicitly given, else the profile's ranked first model, so it is
    /// always populated even on an unpinned launch. Read this for "what model
    /// did the launch target"; read [`Self::model_pin`] for whether an explicit
    /// pin exists.
    pub model_requested: String,
    /// The **explicit** dispatch pin — `spec.model_id` only, with no
    /// profile-default fallback. `None` on an unpinned launch.
    ///
    /// The post-launch pin gate arms from this and this alone: an unpinned
    /// launch must never be refused for departing from a model nobody named.
    /// Distinct from [`Self::model_requested`], which is also set to the
    /// synthetic profile default on unpinned launches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_pin: Option<String>,
}

/// Permission flags actually emitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecipePermission {
    /// Native `--permission-mode` value (`default` for spec `manual`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli_mode: Option<String>,
    /// `--permission-prompts` value, when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompts: Option<String>,
    /// Extra permission flags, e.g. `--allow-dangerously-skip-permissions`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_flags: Vec<String>,
}

/// Durable launch decision. Safe to serialize to SQLite; contains no secrets or prompts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchRecipe {
    /// Launch identity (`launch_` prefix).
    pub launch_id: Id,
    /// Driver that will consume this recipe.
    pub driver: DriverKind,
    /// Pinned binary.
    pub binary: crate::binary::BinaryPin,
    /// Absolute cwd used as a spawn attribute (not a `--cwd` flag).
    pub cwd: String,
    /// Native argv. Never includes prompt text or secret values.
    pub argv: Vec<String>,
    /// Env names/sources allowed at spawn. No values.
    pub env_allowlist: Vec<EnvAllowlistEntry>,
    /// 0600 files written for this launch.
    pub materialized_files: Vec<MaterializedFile>,
    /// `--setting-sources` entries, when applicable.
    pub setting_sources: Vec<String>,
    /// Explicit session id for print/pty new or resume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Registered native home absolute path.
    pub native_home: String,
    /// How the first prompt is delivered.
    pub input_delivery: InputDelivery,
    /// Non-secret provider selection.
    pub provider: RecipeProvider,
    /// Permission flags.
    pub permission: RecipePermission,
    /// Per-launch host capabilities granted on this launch (D-045); empty
    /// means nothing was granted and no capability file may exist.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// MCP servers the granted capabilities registered for this launch.
    #[serde(default)]
    pub mcp_servers: Vec<GrantedMcpServer>,
    /// Technical-debt tags such as [`TECH_DEBT_M0_PERM_01`].
    pub technical_debt: Vec<String>,
    /// Redacted audit record.
    pub audit: LaunchAudit,
}

impl LaunchRecipe {
    /// Delete every [`FileLifetime::Launch`] overlay. Call after the child exits.
    ///
    /// The lifetime was documented as "deleted after the child exits" but nothing
    /// ever removed them (S5), so `settings.json` and the `api-key-helper` script
    /// — the latter holding the per-instance broker token — outlived the run. The
    /// helper is overwritten with zeros before unlinking so the token does not
    /// linger in freed blocks.
    ///
    /// Returns the paths removed. Errors are reported per path rather than
    /// aborting: cleanup runs on a shutdown path and must not mask the exit.
    pub fn cleanup_launch_files(&self) -> Vec<(String, Option<std::io::Error>)> {
        let mut results = Vec::new();
        for file in &self.materialized_files {
            if file.lifetime != FileLifetime::Launch {
                continue;
            }
            results.push((file.path.clone(), shred_file(Path::new(&file.path))));
        }
        results
    }
}

/// Overwrite with zeros, then unlink. `NotFound` is success — already gone.
fn shred_file(path: &Path) -> Option<std::io::Error> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => {
            if let Ok(mut file) = std::fs::OpenOptions::new().write(true).open(path) {
                let _ = file.write_all(&vec![0u8; meta.len() as usize]);
                let _ = file.sync_all();
            }
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => return Some(error),
    }
    match std::fs::remove_file(path) {
        Ok(()) => None,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => Some(error),
    }
}

/// Run [`LaunchRecipe::cleanup_launch_files`] and warn on whatever could not be
/// removed. Drivers call this from `close()`, where an error must not mask the exit.
pub fn report_launch_cleanup(recipe: &LaunchRecipe, driver: &str) {
    for (path, error) in recipe.cleanup_launch_files() {
        if let Some(error) = error {
            tracing::warn!(%driver, %path, %error, "launch overlay cleanup failed");
        }
    }
}
