//! Persistent launch recipe. Serializes without secret values or prompts.

use remuda_protocol::{ApprovalAuthority, BoolLiteral, Digest, DriverKind, Id, InputDelivery};
use serde::{Deserialize, Serialize};

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
    /// Codex/Grok config overlay.
    ProviderConfig,
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
    /// Always `0600` on Unix.
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
    /// Requested model id.
    pub model_requested: String,
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
    /// Technical-debt tags such as [`TECH_DEBT_M0_PERM_01`].
    pub technical_debt: Vec<String>,
    /// Redacted audit record.
    pub audit: LaunchAudit,
}
