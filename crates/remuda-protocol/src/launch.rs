//! Launch wire declarations; `protocol.md`.

use crate::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// ProfileRef; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProfileRef {
    /// `id`; protocol §4.1.
    pub id: Id,
    /// `revision`; protocol §4.1.
    pub revision: U64,
}

/// Public secret metadata for a stored ProviderProfile. The token is never on this type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSecretView {
    /// True when the Hub vault holds a token for this profile.
    pub present: bool,
    /// Last four UTF-8 characters of the token, or null when absent.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub last4: Option<String>,
    /// First 16 hex characters of SHA-256(token), or null when absent.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub fingerprint: Option<String>,
}

/// Operator-configured provider profile (Hub registry). `protocol.md` §4.4 / D-012.
///
/// GET never includes the auth token; only [`ProviderSecretView`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderProfile {
    /// Profile identity (`pvp_` prefix).
    pub id: Id,
    /// Monotonic revision; increments on PATCH (including token rotate).
    pub revision: U64,
    /// Operator label.
    pub name: String,
    /// `gateway` (Anthropic-Messages compatible) or `direct` (provider key).
    pub kind: ProviderProfileKind,
    /// Ingress base URL. Required for [`ProviderProfileKind::Gateway`].
    pub base_url: String,
    /// Catalog model ids this profile may resolve.
    pub models: Vec<String>,
    /// Prefill for New Session when `modelId` is omitted.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub default_model: Option<String>,
    /// Extra HTTP headers for the gateway (never Authorization / x-api-key).
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// When true, New Session `delegation=gateway` selects this profile.
    pub default_gateway: bool,
    /// `universal` or `host:<hostId>` (D-021). Omitted on the wire means universal.
    #[serde(
        default = "universal_scope",
        skip_serializing_if = "is_universal_scope"
    )]
    pub scope: String,
    /// Fingerprint/last4 only.
    pub secret: ProviderSecretView,
    /// Create-time.
    pub created_at: Timestamp,
    /// Update-time.
    pub updated_at: Timestamp,
}

/// Launch overlay the Node writes as Claude `--settings`. The token stays off this type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderOverlaySpec {
    /// Profile identity (`pvp_` prefix).
    pub profile_id: Id,
    /// `gateway` or `direct`.
    pub kind: ProviderProfileKind,
    /// Ingress base URL copied onto `ANTHROPIC_BASE_URL` for gateway.
    pub base_url: String,
    /// Model written into the settings overlay and `--model`.
    pub model: String,
    /// Extra HTTP headers for the gateway. Empty for direct.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// `universal` or `host:<hostId>` (D-021). Omitted on the wire means universal.
    #[serde(
        default = "universal_scope",
        skip_serializing_if = "is_universal_scope"
    )]
    pub scope: String,
}

fn universal_scope() -> String {
    "universal".into()
}

fn is_universal_scope(value: &str) -> bool {
    value.is_empty() || value == "universal"
}

/// SettingsOverlay; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SettingsOverlay {
    /// `format`; protocol §4.1.
    pub format: SettingsFormat,
    /// `object_ref`; protocol §4.1.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub object_ref: Option<Id>,
    /// `revision`; protocol §4.1.
    pub revision: U64,
}

/// NativeHome; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NativeHome {
    /// `mode`; protocol §4.1.
    pub mode: NativeHomeMode,
    /// `store_id`; protocol §4.1.
    pub store_id: Id,
}

/// Native effort selection; `protocol.md` §4.1 (D-028 §9.1).
///
/// Six Codex levels (`low..=ultra`), five Claude levels (`low..=max`), and an
/// orthogonal Claude `ultracode` boolean. `ultracode` is `xhigh` plus dynamic
/// workflow, is session-only, and is never persisted as a level name. It is
/// independent of Codex's `ultra` level.
///
/// [`EffortName`] additionally carries the legacy `minimal` input, which maps
/// to `low` for Codex and is rejected at Claude, agy, and Grok launches.
///
/// Deserialization accepts the pre-D-028 shape `{index, name}` and normalizes
/// legacy tier **names** through [`normalize_legacy_effort`], so a stored row
/// or an old client keeps working. Legacy normalization is **per harness**:
///
/// | legacy `name` | harness | normalized |
/// | --- | --- | --- |
/// | `default` | any | `low` |
/// | `think` | claude | `high` |
/// | `think-hard` | claude | `xhigh` |
/// | `minimal` | codex | `low` |
/// | `ultra` | codex | `ultra` (a real Codex level; rejected by other harnesses) |
/// | `quick` / `standard` / `max` | grok | `low` / `medium` / `xhigh` |
/// | `ultracode` | claude | `xhigh` + `ultracode: true` |
/// | anything unrecognized | any | the harness default (`high` for claude, `medium` otherwise) |
///
/// Normalization is by **name**, never by index: the legacy tables had
/// different lengths per harness, so index 3 meant `ultracode` for Claude and
/// `ultra` for Codex. `index` on the wire is therefore ignored on read and not
/// written back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EffortSelection {
    /// Native level: Codex `low..=ultra`, Claude `low..=max`, Grok `low..=xhigh`.
    pub name: EffortName,
    /// Dynamic-workflow flag (`--effort ultracode`). Session-only.
    pub ultracode: bool,
}

/// The one legacy-name normalizer every crate consumes (Hub store, Node
/// model, driver/web tables) so the per-harness vocabulary cannot diverge
/// again.
///
/// `kind` may be a loose wire string; an unparseable kind is treated as
/// claude (the historical default), exactly like the old kind-less
/// normalizer did.
pub fn normalize_legacy_effort(kind: AgentKind, name: &str) -> EffortSelection {
    match name.trim().to_ascii_lowercase().as_str() {
        // Preserve current words so the driver can reject unsupported levels
        // with its existing InvalidLaunchSpec error, rather than silently
        // downgrading a request for another harness's level.
        "minimal" if kind == AgentKind::Codex => EffortName::Low,
        "minimal" => EffortName::Minimal,
        "low" => EffortName::Low,
        "medium" => EffortName::Medium,
        "high" => EffortName::High,
        "xhigh" => EffortName::Xhigh,
        "ultra" => EffortName::Ultra,
        // claude's real top.
        "max" => {
            // The invented grok table's top word was `max`; grok has no such
            // menu row, so it migrates onto the real top `xhigh`.
            if kind == AgentKind::Grok {
                EffortName::Xhigh
            } else {
                EffortName::Max
            }
        }
        // Cross-harness legacy words, by name not index.
        "default" => EffortName::Low,
        "think" => EffortName::High,
        "think-hard" => EffortName::Xhigh,
        // The invented grok quick/standard/max table (slider pass 1, never a
        // grok vocabulary — grok-build parses low/medium/high/xhigh).
        "quick" if kind == AgentKind::Grok => EffortName::Low,
        "standard" if kind == AgentKind::Grok => EffortName::Medium,
        "ultracode" => {
            return EffortSelection {
                name: EffortName::Xhigh,
                ultracode: true,
            };
        }
        _ => {
            return if kind == AgentKind::Claude {
                EffortSelection::DEFAULT
            } else {
                // The real CLI default for codex/grok is medium.
                EffortSelection {
                    name: EffortName::Medium,
                    ultracode: false,
                }
            };
        }
    }
    .into_selection()
}

impl EffortName {
    fn into_selection(self) -> EffortSelection {
        EffortSelection {
            name: self,
            ultracode: false,
        }
    }
}

/// Parse the agent kind for a loose wire value before normalizing effort.
pub fn effort_kind_from_str(kind: &str) -> AgentKind {
    match kind.trim().to_ascii_lowercase().as_str() {
        "codex" => AgentKind::Codex,
        "grok" => AgentKind::Grok,
        "agy" => AgentKind::Agy,
        "generic" => AgentKind::Generic,
        "terminal" => AgentKind::Terminal,
        _ => AgentKind::Claude,
    }
}

impl EffortSelection {
    /// Default tier: `high`, cost baseline 1.0 (D-028 §9.1).
    pub const DEFAULT: Self = Self {
        name: EffortName::High,
        ultracode: false,
    };

    /// Normalize a legacy or current Claude tier **name** into a selection.
    ///
    /// Equivalent to [`normalize_legacy_effort`] with `AgentKind::Claude`.
    /// Kept for the kind-less call sites; new code should normalize with the
    /// harness kind so codex/grok legacy words (minimal, quick/standard/max)
    /// migrate onto the right native table.
    pub fn from_legacy_name(name: &str) -> Self {
        normalize_legacy_effort(AgentKind::Claude, name)
    }

    /// Wire spelling of the level alone, ignoring `ultracode`; §9.1.
    ///
    /// This is what gets persisted: `ultracode` rides along as its own boolean
    /// so a stored row still records which level it is equivalent to, rather
    /// than collapsing into a name that is not a native level.
    pub fn level_name(&self) -> &'static str {
        match self.name {
            EffortName::Minimal => "minimal",
            EffortName::Low => "low",
            EffortName::Medium => "medium",
            EffortName::High => "high",
            EffortName::Xhigh => "xhigh",
            EffortName::Max => "max",
            EffortName::Ultra => "ultra",
        }
    }

    /// Value for the native `--effort` flag; §9.1.
    ///
    /// `ultracode` replaces the level name because the native flag takes one
    /// value and `--effort ultracode` is the documented (measured) spelling.
    pub fn flag_value(&self) -> &'static str {
        if self.ultracode {
            return "ultracode";
        }
        self.level_name()
    }
}

impl Default for EffortSelection {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl<'de> Deserialize<'de> for EffortSelection {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Wire {
            #[serde(default)]
            name: Option<String>,
            #[serde(default)]
            ultracode: Option<bool>,
        }
        // A bare string ("think") is accepted too: the Hub's instance spec has
        // carried `effortName` as a loose string since before D-028.
        let value = serde_json::Value::deserialize(deserializer)?;
        if let Some(name) = value.as_str() {
            return Ok(Self::from_legacy_name(name));
        }
        let wire: Wire = serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        let Some(name) = wire.name else {
            return Err(serde::de::Error::missing_field("name"));
        };
        let mut selection = Self::from_legacy_name(&name);
        // An explicit `ultracode: true` on a level name that is not itself
        // `ultracode` still sets the flag; `false` never clears the flag the
        // legacy `ultracode` name implies, because that name *is* the request.
        if wire.ultracode == Some(true) {
            selection.ultracode = true;
        }
        Ok(selection)
    }
}

/// Effective effort, read back from a Claude assistant transcript record
/// (`effort` / `perTurnEffort`); D-028 §9.1.
///
/// This is the *observed* tier, never the requested one. Claude reports
/// `ultracode` sessions as level `xhigh` and does not repeat the workflow flag
/// on assistant records, so `ultracode` is `None` unless the observation path
/// has positive evidence (e.g. an immediately preceding `/effort ultracode`
/// switch the driver itself made).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EffortEffective {
    /// Observed level name.
    pub name: EffortName,
    /// Observed dynamic-workflow flag; `None` when the transcript does not
    /// expose it (the common case).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ultracode: Option<bool>,
    /// What established this level.
    pub source: EffortSource,
    /// When the observation was made.
    pub observed_at: Timestamp,
}

/// Claude renderer requested at launch (D-028 §9.2).
///
/// This is intent only; the terminal snapshot's `altScreen` reports the
/// observed screen state after Claude applies platform and accessibility rules.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum TuiMode {
    /// Fullscreen renderer; the launch default when no preference is supplied.
    #[default]
    Fullscreen,
    /// Inline renderer, named `default` by Claude's settings format.
    Default,
}

/// InstanceSpec; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InstanceSpec {
    /// `schema_version`; protocol §4.1.
    pub schema_version: SchemaVersion,
    /// `host`; protocol §4.1.
    pub host: HostId,
    /// `workspace_id`; protocol §4.1.
    pub workspace_id: WorkspaceId,
    /// `kind`; protocol §4.1.
    pub kind: AgentKind,
    /// `driver`; protocol §4.1.
    pub driver: DriverKind,
    /// `binary_ref`; protocol §4.1.
    pub binary_ref: Id,
    /// Host-absolute executable override, Node-validated; §4.1.
    ///
    /// Absent means the Node resolves the driver's default command the way it
    /// always has (host default, then `REMUDA_CLAUDE_BIN`, then `PATH`). The
    /// Hub stores the string but cannot stat the Node's filesystem, so every
    /// containment, ownership, and mode check happens on the Node and a path
    /// that fails them fails the launch — there is no fallback to `PATH`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_path: Option<String>,
    /// Expected digest of [`Self::binary_path`], for pin-on-record; §4.1.
    ///
    /// When present the Node's own pin must equal it or the launch is refused,
    /// so a caller that recorded a binary can detect it changing underneath.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_sha256: Option<Digest>,
    /// `cwd`; protocol §4.1.
    pub cwd: String,
    /// `worktree`; protocol §4.1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeSpec>,
    /// `provider_profile`; protocol §4.1.
    pub provider_profile: ProfileRef,
    /// `model_id`; protocol §4.1.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub model_id: Option<String>,
    /// Native effort selection; §4.1 (D-028 §9.1).
    ///
    /// Absent means "the harness decides"; the materializer emits no
    /// `--effort` flag at all rather than guessing a level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<EffortSelection>,
    /// Requested renderer; omission uses the host default, then fullscreen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tui: Option<TuiMode>,
    /// `permission_mode`; protocol §4.1.
    pub permission_mode: PermissionMode,
    /// `env`; protocol §4.1.
    pub env: BTreeMap<String, EnvBinding>,
    /// `args`; protocol §4.1.
    pub args: Vec<String>,
    /// `settings_overlay`; protocol §4.1.
    pub settings_overlay: SettingsOverlay,
    /// `native_home`; protocol §4.1.
    pub native_home: NativeHome,
    /// `carrier`; protocol §4.1.
    pub carrier: CarrierSpec,
    /// `required_capabilities`; protocol §4.1.
    pub required_capabilities: Vec<CapabilityName>,
    /// `completion_scope`; protocol §4.1.
    pub completion_scope: CompletionScope,
    /// `parent`; protocol §4.1.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub parent: Option<InstanceParent>,
}

/// ProviderSelection; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSelection {
    /// `profile_id`; protocol §4.1.
    pub profile_id: Id,
    /// `profile_revision`; protocol §4.1.
    pub profile_revision: U64,
    /// `endpoint_id`; protocol §4.1.
    pub endpoint_id: Id,
    /// `ingress`; protocol §4.1.
    pub ingress: ProviderIngress,
    /// `credential_ref`; protocol §4.1.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub credential_ref: Option<Id>,
    /// `credential_version`; protocol §4.1.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub credential_version: Option<U64>,
    /// `model_requested`; protocol §4.1.
    pub model_requested: String,
    /// `model_resolved`; protocol §4.1.
    pub model_resolved: Knowledge<String>,
    /// `selection_reason`; protocol §4.1.
    pub selection_reason: SelectionReason,
}

/// PromptInput; `protocol.md` §3.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PromptInput {
    /// `mode`; protocol §3.1.
    pub mode: PromptMode,
    /// `blocks`; protocol §3.1.
    pub blocks: Vec<ContentBlock>,
    /// `origin`; protocol §3.1.
    pub origin: InputOrigin,
    /// `native_client_message_id`; protocol §3.1.
    pub native_client_message_id: String,
}

/// SteerInput; `protocol.md` §3.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SteerInput {
    /// `expected_native_turn_id`; protocol §3.1.
    pub expected_native_turn_id: String,
    /// `blocks`; protocol §3.1.
    pub blocks: Vec<ContentBlock>,
    /// `native_client_message_id`; protocol §3.1.
    pub native_client_message_id: String,
}

/// ModelSwitchInput; `protocol.md` §3.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelSwitchInput {
    /// `model_id`; protocol §3.1.
    pub model_id: String,
    /// `effective`; protocol §3.1.
    pub effective: ModelEffective,
    /// Native effort tier name when the driver supports a runtime switch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

/// DriverInput; `protocol.md` §3.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type")]
pub enum DriverInput {
    /// `prompt` payload; §3.1.
    #[serde(rename = "prompt")]
    Prompt(Box<PromptInput>),
    /// `steer` payload; §3.1.
    #[serde(rename = "steer")]
    Steer(Box<SteerInput>),
    /// `model-switch` payload; §3.1.
    #[serde(rename = "model-switch")]
    ModelSwitch(Box<ModelSwitchInput>),
}

/// SendInput; `protocol.md` §7.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type")]
pub enum SendInput {
    /// `prompt` payload; §7.2.
    #[serde(rename = "prompt")]
    Prompt(Box<PromptInput>),
    /// `steer` payload; §7.2.
    #[serde(rename = "steer")]
    Steer(Box<SteerInput>),
}

/// LiteralEnv; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LiteralEnv {
    /// `value`; protocol §4.1.
    pub value: String,
    /// `visibility`; protocol §4.1.
    pub visibility: EnvVisibility,
}

/// CredentialEnv; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CredentialEnv {
    /// `credential_ref`; protocol §4.1.
    pub credential_ref: Id,
    /// `version`; protocol §4.1.
    pub version: U64,
}

/// HostEnv; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HostEnv {
    /// `name`; protocol §4.1.
    pub name: String,
}

/// EnvBinding; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "source")]
pub enum EnvBinding {
    /// `literal` payload; §4.1.
    #[serde(rename = "literal")]
    Literal(Box<LiteralEnv>),
    /// `credential` payload; §4.1.
    #[serde(rename = "credential")]
    Credential(Box<CredentialEnv>),
    /// `host-env` payload; §4.1.
    #[serde(rename = "host-env")]
    HostEnv(Box<HostEnv>),
}

/// ClaudePermission; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClaudePermission {
    /// `mode`; protocol §4.1.
    pub mode: ClaudePermissionMode,
    /// `interaction`; protocol §4.1.
    pub interaction: ClaudeInteractionMode,
}

/// CodexPermission; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CodexPermission {
    /// `approval_policy`; protocol §4.1.
    pub approval_policy: ApprovalPolicy,
    /// `approvals_reviewer`; protocol §4.1.
    pub approvals_reviewer: ApprovalsReviewer,
    /// `execution`; protocol §4.1.
    pub execution: CodexExecution,
}

/// SandboxExecution; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct SandboxExecution {
    /// `sandbox`; protocol §4.1.
    pub sandbox: SandboxMode,
}

/// NamedPermissions; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct NamedPermissions {
    /// `permissions`; protocol §4.1.
    pub permissions: String,
}

/// Mutually exclusive Codex sandbox or permission profile; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum CodexExecution {
    /// Built-in sandbox choice.
    Sandbox(SandboxExecution),
    /// Named native permissions profile.
    Permissions(NamedPermissions),
}

/// GrokPermission; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GrokPermission {
    /// `mode`; protocol §4.1.
    pub mode: GrokPermissionMode,
}

/// AgyPermission; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgyPermission {
    /// `mode`; protocol §4.1.
    pub mode: AgyPermissionMode,
}

/// GenericPermission; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GenericPermission {
    /// `mode`; protocol §4.1.
    pub mode: GenericPermissionMode,
}

/// PermissionMode; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind")]
pub enum PermissionMode {
    /// `claude` payload; §4.1.
    #[serde(rename = "claude")]
    Claude(Box<ClaudePermission>),
    /// `codex` payload; §4.1.
    #[serde(rename = "codex")]
    Codex(Box<CodexPermission>),
    /// `grok` payload; §4.1.
    #[serde(rename = "grok")]
    Grok(Box<GrokPermission>),
    /// `agy` payload; §4.1.
    #[serde(rename = "agy")]
    Agy(Box<AgyPermission>),
    /// `generic` payload; §4.1.
    #[serde(rename = "generic")]
    Generic(Box<GenericPermission>),
}

/// ExistingWorktree; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExistingWorktree {
    /// `worktree_id`; protocol §4.1.
    pub worktree_id: WorktreeId,
}

/// CreateWorktree; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateWorktree {
    /// `worktree_id`; protocol §4.1.
    pub worktree_id: WorktreeId,
    /// `base_oid`; protocol §4.1.
    pub base_oid: String,
    /// `branch`; protocol §4.1.
    pub branch: String,
}

/// WorktreeSpec; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "mode")]
pub enum WorktreeSpec {
    /// `existing` payload; §4.1.
    #[serde(rename = "existing")]
    Existing(Box<ExistingWorktree>),
    /// `create` payload; §4.1.
    #[serde(rename = "create")]
    Create(Box<CreateWorktree>),
}

/// Herdr-backed PTY launch, pinned before dispatch; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PtyCarrier {
    /// Phase 0 production PTYs are hosted by Herdr.
    pub backend: PtyBackend,
    /// Resolved binary and server lifetime; no ambient default server.
    pub server: HerdrServer,
    /// Explicit Herdr session to own the new pane.
    pub session: String,
}

/// Deferred first input for Claude background launch; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClaudeBgCarrier {
    /// Initial input starts the job only after its send Command is durable.
    pub input_delivery: BgInputDelivery,
    /// Requires explicit human opt-in for one text block; bots cannot use it.
    pub argv_input_policy: ArgvInputPolicy,
}

/// Native process carrier; changing it requires a new Instance; `protocol.md` §4.1.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum CarrierSpec {
    /// Native structured stdio transport.
    #[serde(rename = "stdio")]
    Stdio,
    /// Herdr owns the PTY process and pane.
    #[serde(rename = "pty")]
    Pty(Box<PtyCarrier>),
    /// Native daemon job; opening an attach pane is a separate explicit Command.
    #[serde(rename = "claude-bg")]
    ClaudeBg(Box<ClaudeBgCarrier>),
    /// Login `$SHELL` in a local PTY (`shell-pty` / kind `terminal`).
    #[serde(rename = "shell-pty")]
    ShellPty,
}

impl<'de> Deserialize<'de> for CarrierSpec {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut fields = serde_json::Map::<String, serde_json::Value>::deserialize(deserializer)?;
        let kind = fields
            .remove("type")
            .ok_or_else(|| serde::de::Error::missing_field("type"))?;
        match kind.as_str() {
            Some("stdio") if fields.is_empty() => Ok(Self::Stdio),
            Some("stdio") => Err(serde::de::Error::custom("stdio carrier accepts only type")),
            Some("pty") => serde_json::from_value(serde_json::Value::Object(fields))
                .map(Box::new)
                .map(Self::Pty)
                .map_err(serde::de::Error::custom),
            Some("claude-bg") => serde_json::from_value(serde_json::Value::Object(fields))
                .map(Box::new)
                .map(Self::ClaudeBg)
                .map_err(serde::de::Error::custom),
            Some("shell-pty") if fields.is_empty() => Ok(Self::ShellPty),
            Some("shell-pty") => Err(serde::de::Error::custom(
                "shell-pty carrier accepts only type",
            )),
            _ => Err(serde::de::Error::custom("unknown carrier type")),
        }
    }
}
