//! The `launch.capabilities` journal event; `protocol.md` §5.5 (cb-seat-profile,
//! D-063, main-agent.md §3.4.2).
//!
//! One per agent launch, written by the Node before the harness starts. It
//! names what the launch was asked for, what it materialized, what it skipped
//! by design and what could not reach the harness. Only names, paths relative
//! to a launch root, digests and categories: never an env value, an MCP
//! server's command, args or env, a path outside the home, or file contents.

use crate::{AgentKind, DriverKind, U64};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

wire_enum!(LaunchCapabilitiesLaunch, "5.5", {
    Create => "create",
    Resume => "resume",
});

wire_enum!(LaunchItemClass, "5.5", {
    Skill => "skill",
    Capability => "capability",
    McpServer => "mcp-server",
    McpConfig => "mcp-config",
    Grant => "grant",
});

wire_enum!(LaunchItemSource, "5.5", {
    Embedded => "embedded",
    Spec => "spec",
    Hub => "hub",
    Args => "args",
});

wire_enum!(LaunchItemRoot, "5.5", {
    NativeHome => "native-home",
    LaunchDir => "launch-dir",
});

wire_enum!(LaunchItemVia, "5.5", {
    Tree => "tree",
    Flag => "flag",
    DataBlock => "data-block",
    Conversation => "conversation",
    Argv => "argv",
    CodexConfig => "codex-config",
});

wire_enum!(LaunchSkipReason, "5.5", {
    OperatorHome => "operator-home",
    NoSkillReader => "no-skill-reader",
    UserOwned => "user-owned",
});

wire_enum!(LaunchUnavailableReason, "5.5", {
    Unreadable => "unreadable",
    Missing => "missing",
    NotADirectory => "not-a-directory",
    NotAFile => "not-a-file",
    TooLarge => "too-large",
    InvalidName => "invalid-name",
    UnsupportedByHarness => "unsupported-by-harness",
    NotPermitted => "not-permitted",
});

impl LaunchUnavailableReason {
    /// Whether this category refuses the launch. Only a configured item that
    /// exists but cannot be read does (main-agent.md §3.4.2).
    #[must_use]
    pub fn refuses(self) -> bool {
        self == Self::Unreadable
    }
}

/// The refusal reason a launch with an unreadable configured item carries.
pub const LAUNCH_CAPABILITY_UNREADABLE: &str = "launch-capability-unreadable";

/// One configured item; §5.5.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LaunchConfiguredItem {
    /// What kind of item.
    pub class: LaunchItemClass,
    /// Its name: a skill directory's base name, a grant verb, a capability
    /// or MCP server name, an MCP config file's base name or `inline`.
    pub name: String,
    /// Who asked for it.
    pub source: LaunchItemSource,
}

/// One materialized item; §5.5.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LaunchResolvedItem {
    /// What kind of item.
    pub class: LaunchItemClass,
    /// Its name, as in `configured`.
    pub name: String,
    /// The launch root [`Self::path`] is relative to; null when the item has
    /// no file of its own under a root (a grant, a caller `--mcp-config`).
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub root: Option<LaunchItemRoot>,
    /// Path relative to [`Self::root`]; never absolute.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub path: Option<String>,
    /// `sha256:<hex>` of the bytes (for a tree, `skill_tree_digest`).
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub digest: Option<String>,
    /// How the item rides the launch.
    pub via: LaunchItemVia,
}

/// One designed skip; §5.5. Never an error and never a refusal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LaunchSkippedItem {
    /// What kind of item.
    pub class: LaunchItemClass,
    /// Its name, as in `configured`.
    pub name: String,
    /// Why it was skipped.
    pub reason: LaunchSkipReason,
}

/// One item that will not reach the harness; §5.5.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LaunchUnavailableItem {
    /// What kind of item.
    pub class: LaunchItemClass,
    /// Its name, as in `configured`.
    pub name: String,
    /// Why.
    pub reason: LaunchUnavailableReason,
}

/// What to do about a refused launch; the shape of a Hub error's `next`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LaunchRefusalNext {
    /// `ask-owner`: the configuration is the owner's to fix.
    pub action: String,
    /// One sentence for the reader.
    pub hint: String,
}

/// Why the Node refused the launch; §5.5.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LaunchRefusal {
    /// [`LAUNCH_CAPABILITY_UNREADABLE`].
    pub reason: String,
    /// The next step.
    pub next: LaunchRefusalNext,
}

/// `launch.capabilities` payload; §5.5.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LaunchCapabilitiesPayload {
    /// `instance.create` or `instance.resume`.
    pub launch: LaunchCapabilitiesLaunch,
    /// The harness kind.
    pub harness: AgentKind,
    /// The carrier.
    pub driver: DriverKind,
    /// The lineage profile revision this chapter applied; null without one.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub profile_revision: Option<U64>,
    /// What the spec and the Hub asked for.
    pub configured: Vec<LaunchConfiguredItem>,
    /// What is materialized.
    pub resolved: Vec<LaunchResolvedItem>,
    /// Designed skips.
    pub skipped: Vec<LaunchSkippedItem>,
    /// What will not reach the harness.
    pub unavailable: Vec<LaunchUnavailableItem>,
    /// Set when the Node refuses the launch.
    #[serde(deserialize_with = "crate::scalar::required_option")]
    pub refusal: Option<LaunchRefusal>,
}

impl LaunchCapabilitiesPayload {
    /// Whether any unavailable item refuses the launch.
    #[must_use]
    pub fn refuses(&self) -> bool {
        self.unavailable.iter().any(|item| item.reason.refuses())
    }
}
