//! Enums wire declarations; `protocol.md`.

use serde::{Deserialize, Serialize};
use std::str::FromStr as _;

use crate::{HostId, WireValueError};

wire_enum!(AgentKind, "1.3", {
    Claude => "claude",
    Codex => "codex",
    Grok => "grok",
    Agy => "agy",
    Generic => "generic",
    Terminal => "terminal",
});

wire_enum!(DriverKind, "3.1", {
    ClaudePrint => "claude-print",
    ClaudeSdk => "claude-sdk",
    ClaudePty => "claude-pty",
    ClaudeBg => "claude-bg",
    CodexAppserver => "codex-appserver",
    GrokAcp => "grok-acp",
    AgyPrint => "agy-print",
    GenericPty => "generic-pty",
    ShellPty => "shell-pty",
});

wire_enum!(HostState, "2.1", {
    Enrolled => "enrolled",
    Online => "online",
    Offline => "offline",
    Reconciling => "reconciling",
    Retired => "retired",
});

wire_enum!(PathStyle, "2.1", {
    Posix => "posix",
    Windows => "windows",
});

wire_enum!(HostTransportMode, "2.1", {
    OutboundWss => "outbound-wss",
    SshTunnel => "ssh-tunnel",
});

wire_enum!(WorkspaceState, "2.2", {
    Registering => "registering",
    Ready => "ready",
    Unavailable => "unavailable",
    Archived => "archived",
    Failed => "failed",
});

wire_enum!(WritePolicy, "2.2", {
    Exclusive => "exclusive",
    IsolatedWorktree => "isolated-worktree",
    SharedExplicit => "shared-explicit",
});

wire_enum!(WorktreeOwner, "2.2", {
    Runtime => "runtime",
    Native => "native",
    External => "external",
});

wire_enum!(WorktreeState, "2.2", {
    Creating => "creating",
    Ready => "ready",
    Unavailable => "unavailable",
    Removed => "removed",
    Failed => "failed",
});

wire_enum!(InstanceLifecycle, "2.3", {
    Requested => "requested",
    Preparing => "preparing",
    Starting => "starting",
    Ready => "ready",
    Closing => "closing",
    Exited => "exited",
    Failed => "failed",
    Unknown => "unknown",
    Reconciling => "reconciling",
});

wire_enum!(Activity, "2.3", {
    Idle => "idle",
    Working => "working",
    WaitingInteraction => "waiting-interaction",
    Draining => "draining",
});

wire_enum!(InstanceMode, "2.3", {
    Native => "native",
    Promoted => "promoted",
});

wire_enum!(LaunchedBy, "2.3", {
    Remuda => "remuda",
    User => "user",
});

// Delegation-tree grant verbs; design §2.5. Enforcement reads these, never
// the preset name stored as `role`.
wire_enum!(GrantVerb, "2.5", {
    Dispatch => "dispatch",
    Land => "land",
    Spend => "spend",
    AddressOwner => "address-owner",
});

wire_enum!(Connectivity, "2.3", {
    Connected => "connected",
    Disconnected => "disconnected",
    Reconciling => "reconciling",
});

wire_enum!(Ownership, "2.3", {
    Managed => "managed",
    AdoptedControl => "adopted-control",
    ObservedOnly => "observed-only",
});

wire_enum!(ActorType, "1.1", {
    Human => "human",
    Bot => "bot",
    Agent => "agent",
    System => "system",
});

wire_enum!(RunCauseType, "2.4", {
    Command => "command",
    NativeContinuation => "native-continuation",
    ExternalInput => "external-input",
    Import => "import",
});

wire_enum!(Parentage, "2.4", {
    KnownRoot => "known-root",
    Linked => "linked",
    Unknown => "unknown",
});

wire_enum!(CompletionScope, "2.4", {
    NativeTurn => "native-turn",
    Task => "task",
});

wire_enum!(RunState, "2.4", {
    Queued => "queued",
    Running => "running",
    WaitingInteraction => "waiting-interaction",
    Draining => "draining",
    Succeeded => "succeeded",
    Failed => "failed",
    Cancelled => "cancelled",
    Unknown => "unknown",
    Reconciling => "reconciling",
});

wire_enum!(StateConfidence, "2.4", {
    Confirmed => "confirmed",
    Unknown => "unknown",
});

wire_enum!(CommandOrigin, "2.5", {
    Ui => "ui",
    Bot => "bot",
    Mcp => "mcp",
    Cli => "cli",
    System => "system",
});

wire_enum!(CommandOperation, "2.5", {
    InstanceCreate => "instance.create",
    InstanceAttach => "instance.attach",
    InstanceOpenTerminal => "instance.open_terminal",
    InstanceResume => "instance.resume",
    InstanceSend => "instance.send",
    InstanceCancel => "instance.cancel",
    InstanceClose => "instance.close",
    InstanceFork => "instance.fork",
    InstanceConfigure => "instance.configure",
    InteractionRespond => "interaction.respond",
    TtyWrite => "tty.write",
    WorkspaceRegister => "workspace.register",
    WorktreeCreate => "worktree.create",
    WorktreeRemove => "worktree.remove",
});

wire_enum!(CommandState, "2.5", {
    Queued => "queued",
    Accepted => "accepted",
    Settled => "settled",
});

wire_enum!(CommandAuthority, "2.5", {
    HubInbox => "hub-inbox",
    NodeLedger => "node-ledger",
});

wire_enum!(DispatchState, "2.5", {
    NotDispatched => "not-dispatched",
    IntentDurable => "intent-durable",
    TransportWritten => "transport-written",
    NativeAcknowledged => "native-acknowledged",
});

wire_enum!(ResolutionState, "2.5", {
    Clear => "clear",
    Unknown => "unknown",
    Reconciling => "reconciling",
});

wire_enum!(AcceptanceScope, "2.5", {
    NativeInput => "native-input",
    NativeControl => "native-control",
    RuntimeResource => "runtime-resource",
    TtyBytes => "tty-bytes",
});

wire_enum!(SettlementOutcome, "2.5", {
    Completed => "completed",
    Rejected => "rejected",
    Cancelled => "cancelled",
    Expired => "expired",
});

wire_enum!(InteractionKind, "2.6", {
    Approval => "approval",
    Question => "question",
    PlanReview => "plan-review",
    Elicitation => "elicitation",
});

wire_enum!(InteractionState, "2.6", {
    Pending => "pending",
    AnswerCommitted => "answer-committed",
    Resolved => "resolved",
    Expired => "expired",
    Invalidated => "invalidated",
    Unknown => "unknown",
    Reconciling => "reconciling",
});

wire_enum!(InteractionCarrier, "2.6", {
    ClaudeControl => "claude-control",
    ClaudeHook => "claude-hook",
    HarnessHook => "harness-hook",
    CodexRpc => "codex-rpc",
    AcpRpc => "acp-rpc",
    NativeTty => "native-tty",
    Unsupported => "unsupported",
});

wire_enum!(DeadlineSource, "2.6", {
    Native => "native",
    RuntimePolicy => "runtime-policy",
    None => "none",
    Unknown => "unknown",
});

wire_enum!(DeliveryState, "2.6", {
    NotSent => "not-sent",
    IntentDurable => "intent-durable",
    Written => "written",
    Confirmed => "confirmed",
    Rejected => "rejected",
    Unknown => "unknown",
});

wire_enum!(InteractionResolutionReason, "2.6", {
    Answered => "answered",
    NativeCleared => "native-cleared",
    NativeCancelled => "native-cancelled",
    GenerationEnded => "generation-ended",
    TimedOut => "timed-out",
});

wire_enum!(CapabilityName, "3.2", {
    Resume => "resume",
    Steer => "steer",
    Queue => "queue",
    Interrupt => "interrupt",
    ModelSwitch => "model-switch",
    Fork => "fork",
    StructuredWorkflow => "structured-workflow",
    Artifact => "artifact",
    TtyAttach => "tty-attach",
    Hooks => "hooks",
    InteractiveApproval => "interactive-approval",
    Question => "question",
    PlanReview => "plan-review",
    Elicitation => "elicitation",
    LiveAttach => "live-attach",
    CompletionNativeTurn => "completion-native-turn",
    CompletionTask => "completion-task",
});

wire_enum!(CapabilityState, "3.2", {
    Supported => "supported",
    Unsupported => "unsupported",
    Unknown => "unknown",
});

wire_enum!(CapabilityProvision, "3.2", {
    Native => "native",
    Emulated => "emulated",
    Unknown => "unknown",
});

wire_enum!(EvidenceType, "3.2", {
    Fixture => "fixture",
    NativeNegotiation => "native-negotiation",
    Source => "source",
    Help => "help",
});

wire_enum!(NativeRequestValueType, "1.3", {
    String => "string",
    Number => "number",
});

wire_enum!(SignalTier, "1.3", {
    Hook => "hook",
    File => "file",
    Osc => "osc",
    Screen => "screen",
    None => "none",
});

wire_enum!(AttachMode, "3.1", {
    Observe => "observe",
    Control => "control",
});

wire_enum!(PromptMode, "3.1", {
    NewTurn => "new-turn",
    Steer => "steer",
    Queue => "queue",
});

wire_enum!(InputOrigin, "3.1", {
    Human => "human",
    Bot => "bot",
    Agent => "agent",
});

wire_enum!(ModelEffective, "3.1", {
    NextTurn => "next-turn",
});

// Native reasoning-effort vocabulary shared by the harness CLIs.
//
// Codex exposes `low..=ultra`; `minimal` is retained as an input alias for
// Codex `low`. Claude exposes `low..=max`; its `ultracode` workflow flag is
// separate from Codex's `ultra` level. The driver gates each harness's levels.
wire_enum!(EffortName, "4.1", {
    Minimal => "minimal",
    Low => "low",
    Medium => "medium",
    High => "high",
    Xhigh => "xhigh",
    Max => "max",
    Ultra => "ultra",
});

// Where an *effective* effort observation came from; D-028 §9.1:
// `launch` = read back after a `--effort` launch flag; `slash` = the user
// typed `/effort` in the native TUI; `remuda` = a Remuda-initiated switch;
// `unknown` = observed with no attributable switch.
wire_enum!(EffortSource, "9.1", {
    Launch => "launch",
    Slash => "slash",
    Remuda => "remuda",
    Unknown => "unknown",
});

// Where an *effective* permission-mode observation came from; mirrors
// [`EffortSource`]. `launch` = read back after a `--permission-mode` launch
// flag; `slash` = the user typed `/plan` (or a future mode command) in the
// native TUI; `remuda` = a Remuda shift+tab push-down; `unknown` = observed
// (e.g. a bare shift+tab) with no attributable switch.
wire_enum!(PermissionSource, "9.1", {
    Launch => "launch",
    Slash => "slash",
    Remuda => "remuda",
    Unknown => "unknown",
});

wire_enum!(ClaudePermissionMode, "4.1", {
    Manual => "manual",
    Auto => "auto",
    AcceptEdits => "acceptEdits",
    DontAsk => "dontAsk",
    Plan => "plan",
    BypassPermissions => "bypassPermissions",
});

wire_enum!(ClaudeInteractionMode, "4.1", {
    Host => "host",
    NativeTty => "native-tty",
});

wire_enum!(ApprovalPolicy, "4.1", {
    Untrusted => "untrusted",
    OnRequest => "on-request",
    Never => "never",
});

wire_enum!(ApprovalsReviewer, "4.1", {
    User => "user",
});

wire_enum!(SandboxMode, "4.1", {
    ReadOnly => "read-only",
    WorkspaceWrite => "workspace-write",
    DangerFullAccess => "danger-full-access",
});

wire_enum!(GrokPermissionMode, "4.1", {
    NativePrompt => "native-prompt",
    Auto => "auto",
    AlwaysApprove => "always-approve",
});

wire_enum!(AgyPermissionMode, "4.1", {
    Native => "native",
    AcceptEdits => "accept-edits",
    Plan => "plan",
    AlwaysProceed => "always-proceed",
});

wire_enum!(GenericPermissionMode, "4.1", {
    Native => "native",
});

wire_enum!(SettingsFormat, "4.1", {
    ClaudeJson => "claude-json",
    CodexToml => "codex-toml",
    GrokToml => "grok-toml",
    AgyJson => "agy-json",
    None => "none",
});

wire_enum!(NativeHomeMode, "4.1", {
    Registered => "registered",
});

wire_enum!(PtyBackend, "4.1", {
    Herdr => "herdr",
});

wire_enum!(ProviderIngress, "4.1", {
    AnthropicMessages => "anthropic-messages",
    OpenaiResponses => "openai-responses",
    OpenaiChat => "openai-chat",
    GeminiNative => "gemini-native",
    NativeLogin => "native-login",
});

wire_enum!(ProviderProfileKind, "4.4", {
    Gateway => "gateway",
    Direct => "direct",
});

// ── API routing (D-047 / D-048, 2026-09-19) ────────────────────────────────

/// Wire value of [`ProviderDeliveryMode`] `via`: route this session's model API
/// egress through [`ProviderDelivery::via_host_id`].
pub const PROVIDER_DELIVERY_VIA: &str = "via";
/// Wire value of [`ProviderDeliveryMode`] `direct`; the default, and what an
/// absent `delivery` on the wire means.
pub const PROVIDER_DELIVERY_DIRECT: &str = "direct";

/// Delivery mode of one provider profile; `protocol.md` §4.4 (D-047).
///
/// `direct` (D2 default) is today's behaviour: `baseUrl` plus the credential
/// travel to the worker host. `via` egresses every model API request of the
/// session on a named host instead, and never falls back to `direct` — a
/// reroute would push the request onto a machine the operator excluded.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Default,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderDeliveryMode {
    /// No proxy: the worker host is the egress host. The D2 default.
    #[default]
    Direct,
    /// Egress on the host named by [`ProviderDelivery::via_host_id`].
    Via,
}

impl ProviderDeliveryMode {
    /// Canonical wire spelling (identical to the serde rendering).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Direct => PROVIDER_DELIVERY_DIRECT,
            Self::Via => PROVIDER_DELIVERY_VIA,
        }
    }
}

/// Route a `via` delivery takes between the worker host `W` and the proxy host
/// `H`; `protocol.md` §4.4 (D-047, Amendment A1).
///
/// Decided **once at launch** and echoed by the Node as [`ApiRouteKind`]. A
/// failing `DirectNet` mid-session is never silently re-routed to `HubRelay`:
/// the request fails and the instance reports `blocked{api-route-down}`.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Default,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum ApiRouteMode {
    /// Try the direct network path to `H` when `H` has a configured relay bind,
    /// otherwise relay through the Hub host. The default.
    #[default]
    Auto,
    /// Always in-band over the existing Hub↔Node link.
    HubRelay,
    /// Require the direct network path; refuse at launch with
    /// `api-via-unreachable` when the probe fails.
    DirectNet,
}

impl ApiRouteMode {
    /// Canonical wire spelling (identical to the serde rendering).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::HubRelay => "hub-relay",
            Self::DirectNet => "direct-net",
        }
    }
}

/// The route a `via` session **actually** took, as echoed by the Node; D-035
/// rule 4 (`protocol.md` §4.4).
///
/// Only the two resolved routes exist on this type: `auto` is a request, never
/// an observation, so a record that says `auto` would be the Hub reporting what
/// it asked for instead of what ran.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum ApiRouteKind {
    /// Request bytes went over the network to `H`'s relay endpoint.
    DirectNet,
    /// Request bytes rode the Hub↔Node link to `H` (or the Hub process).
    HubRelay,
}

impl ApiRouteKind {
    /// Canonical wire spelling (identical to the serde rendering).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DirectNet => "direct-net",
            Self::HubRelay => "hub-relay",
        }
    }
}

/// Provider delivery of one profile; `protocol.md` §4.4 (D-047).
///
/// The wire shape is nested, not the CLI's `direct` / `via:<hostId>` spelling:
/// the sub-mode has nowhere to live in a single keyword. An absent `delivery`
/// is [`ProviderDelivery::default`] — `{mode: direct, route: auto}` — so a
/// profile row written before this type existed keeps parsing.
///
/// Deserialization is strict on the one combination that cannot be honoured:
/// `mode: via` with no `via_host_id` is a parse error, not a default. Unknown
/// enum values are parse errors too, so a typo'd route cannot quietly become
/// `auto` and move a session's egress to a machine the operator did not name.
/// [`Self::is_valid`] stays as the runtime check for a value built in Rust.
///
/// The `serde` attribute is for the **schema** only: `Deserialize` is
/// hand-written below (it must reject `via` with no host), so nothing here
/// reads this attribute at runtime — but schemars does, and without it the
/// generated schema would claim to accept unknown keys when the wire struct
/// refuses them.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderDelivery {
    /// `direct` or `via`.
    pub mode: ProviderDeliveryMode,
    /// Proxy host (`hst_…`). Required by `via`, meaningless for `direct`.
    ///
    /// `via` naming the worker host itself is **not** rewritten here: it
    /// collapses to `direct` at decision time, in the Hub, where the placed
    /// host is known. The wire keeps what the operator asked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via_host_id: Option<HostId>,
    /// Route between the worker host and [`Self::via_host_id`]. Ignored by
    /// `direct`, but always serialized so the stored row is self-describing.
    pub route: ApiRouteMode,
}

/// Deserialization shape for [`ProviderDelivery`].
///
/// A separate type because the rejection cannot be expressed as a serde field
/// attribute, and it must not become a schema keyword: `mode: via` without a
/// host is a cross-field rule, and the generated schema documents both fields
/// independently (the write-body contract that `route` may be absent is stated
/// in the OpenAPI operation, not in this derived schema).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProviderDeliveryWire {
    #[serde(default)]
    mode: ProviderDeliveryMode,
    #[serde(default)]
    via_host_id: Option<HostId>,
    #[serde(default)]
    route: ApiRouteMode,
}

impl TryFrom<ProviderDeliveryWire> for ProviderDelivery {
    type Error = WireValueError;
    fn try_from(wire: ProviderDeliveryWire) -> Result<Self, Self::Error> {
        let delivery = Self {
            mode: wire.mode,
            via_host_id: wire.via_host_id,
            route: wire.route,
        };
        // `via` with no host names no machine to egress on. Accepting it would
        // defer the failure to launch time, where the operator has already
        // been told the dispatch was taken.
        if delivery.is_valid() {
            Ok(delivery)
        } else {
            Err(WireValueError(
                "delivery mode `via` requires `viaHostId`".into(),
            ))
        }
    }
}

impl<'de> Deserialize<'de> for ProviderDelivery {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        ProviderDeliveryWire::deserialize(deserializer)
            .and_then(|wire| ProviderDelivery::try_from(wire).map_err(D::Error::custom))
    }
}

impl ProviderDelivery {
    /// `{mode: direct, route: auto}`; §4.4 (D2 default).
    #[must_use]
    pub fn direct() -> Self {
        Self::default()
    }

    /// A `via` delivery to `host_id`.
    #[must_use]
    pub fn via(host_id: HostId, route: ApiRouteMode) -> Self {
        Self {
            mode: ProviderDeliveryMode::Via,
            via_host_id: Some(host_id),
            route,
        }
    }

    /// True when this delivery proxies through another host.
    #[must_use]
    pub const fn is_via(&self) -> bool {
        matches!(self.mode, ProviderDeliveryMode::Via)
    }

    /// The one combination that cannot be honoured: `via` with no host.
    ///
    /// Also the PATCH-time check, so a refusal is a 400 on the request rather
    /// than a launch that fails later.
    #[must_use]
    pub const fn is_valid(&self) -> bool {
        !(matches!(self.mode, ProviderDeliveryMode::Via) && self.via_host_id.is_none())
    }

    /// True for the D2 default, which is omitted on the wire; §4.4.
    ///
    /// A `route` on a `direct` delivery is not the default — it is a value the
    /// operator set — so it keeps the field, and with it the operator's intent.
    #[must_use]
    pub fn is_direct_default(&self) -> bool {
        self.mode == ProviderDeliveryMode::Direct
            && self.via_host_id.is_none()
            && self.route == ApiRouteMode::Auto
    }
}

/// Route the launch **requested**, carried on the instance spec so the Node and
/// the operator's surfaces can see it before the Node answers; §4.4 (D-047).
///
/// This is intent. [`ApiRoute`] is the observation the Node echoes back, and the
/// only one a UI may render as fact (D-035).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RequestedApiRoute {
    /// `direct` when this session does not proxy, `via` when it does.
    #[serde(default)]
    pub mode: ProviderDeliveryMode,
    /// Proxy host (`hst_…`); present only for `via`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via_host_id: Option<HostId>,
    /// Route to attempt first.
    #[serde(default)]
    pub route: ApiRouteMode,
}

/// The API route an instance is actually using; `protocol.md` §2.3 (D-047).
///
/// Stored on the instance projection next to `providerSource` /
/// `providerSourceHint` so `remuda watch` and the Session strip report what
/// ran, never what was asked for (D-035).
///
/// The two halves come from different places, which is worth keeping straight:
/// the Node decides and reports [`Self::mode`], [`Self::route`] and
/// [`Self::via_host_id`] — it is the machine that bound a listener and probed
/// the path — while [`Self::via_host_label`] is the Hub's own registry label,
/// attached on write-back so a surface can name the host without a second
/// lookup. A Hub that takes the request's word for the route would be
/// reporting intent as observation, which is the failure this type exists to
/// make impossible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApiRoute {
    /// `direct` (no proxy) or `via`.
    #[serde(default)]
    pub mode: ProviderDeliveryMode,
    /// Resolved route; present only when [`Self::mode`] is `via`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<ApiRouteKind>,
    /// Proxy host (`hst_…`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via_host_id: Option<HostId>,
    /// Operator-facing label of the proxy host, so the strip can name it
    /// without a second lookup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via_host_label: Option<String>,
}

impl ApiRoute {
    /// `{mode: direct}`; no proxy.
    #[must_use]
    pub fn direct() -> Self {
        Self {
            mode: ProviderDeliveryMode::Direct,
            route: None,
            via_host_id: None,
            via_host_label: None,
        }
    }

    /// A `via` route to `host_id` that took `route`.
    #[must_use]
    pub fn via(host_id: HostId, label: Option<String>, route: ApiRouteKind) -> Self {
        Self {
            mode: ProviderDeliveryMode::Via,
            route: Some(route),
            via_host_id: Some(host_id),
            via_host_label: label,
        }
    }

    /// True when this instance proxies its model API egress.
    #[must_use]
    pub const fn is_via(&self) -> bool {
        matches!(self.mode, ProviderDeliveryMode::Via)
    }
}

/// Host-level relay bind for direct-network routing; `protocol.md` §2.1
/// (D-047, Amendment A1).
///
/// Present on a host only when the operator configured one. Absent means the
/// proxy host's relay listener stays loopback-only and every `via` session
/// takes `hub-relay`, which is what keeps D-031 intact: no listener opens on a
/// non-loopback address without an explicit operator setting, and a bind is an
/// address to serve on — never a tunnel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HostRelayBind {
    /// Explicit bind address (`host:port`). Never `0.0.0.0` and never public:
    /// the Hub refuses those, and the Node refuses to bind them.
    pub addr: String,
    /// Source addresses the listener accepts, by CIDR or exact address. Empty
    /// means "whatever the operator's firewall allows" and is the stricter
    /// reading, not an any-address allowance.
    #[serde(default)]
    pub allow_from: Vec<String>,
}

/// Why a `via` dispatch was refused; `protocol.md` §4.4 (D-047, §B.5).
///
/// Stable lowercase codes in the §9.2 reason vocabulary. Each is a **refusal**,
/// and a refusal is the whole point: there is no code here for "fell back to
/// direct", because no such path may exist. A `via` request that cannot be
/// honoured fails, so a request never reaches a machine the operator excluded
/// and the UI never has to lie about where it went.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum ApiViaRefusal {
    /// The named host is not in the registry. Wire value `api-via-unknown-host`.
    ApiViaUnknownHost,
    /// The named host is enrolled but has no live Hub↔Node session. Checked
    /// before any name, port, or worktree allocation.
    /// Wire value `api-via-host-offline`.
    ApiViaHostOffline,
    /// The named host's Node predates `api.*` and cannot serve as a proxy.
    /// Never downgraded to `direct` (D-035). Wire value `api-via-unsupported`.
    ApiViaUnsupported,
    /// `route: direct-net` was required and the probe to the proxy host's relay
    /// bind failed or timed out (Amendment A1).
    /// Wire value `api-via-unreachable`.
    ApiViaUnreachable,
}

impl ApiViaRefusal {
    /// Canonical wire spelling (identical to the serde rendering).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ApiViaUnknownHost => "api-via-unknown-host",
            Self::ApiViaHostOffline => "api-via-host-offline",
            Self::ApiViaUnsupported => "api-via-unsupported",
            Self::ApiViaUnreachable => "api-via-unreachable",
        }
    }

    /// HTTP status the Hub answers with; §B.5.
    ///
    /// `ApiViaUnknownHost` is a bad request — the caller named something that
    /// does not exist — while the rest are conflicts with current state that a
    /// retry may clear, so the surfaces stay distinguishable without parsing
    /// the message.
    #[must_use]
    pub const fn status(self) -> u16 {
        match self {
            Self::ApiViaUnknownHost => 400,
            Self::ApiViaHostOffline | Self::ApiViaUnsupported | Self::ApiViaUnreachable => 409,
        }
    }
}

/// Roster reason for a session whose proxy host went away mid-flight; §B.5.
///
/// A `WorkerWatchStatus::Blocked` reason, not a dispatch refusal: the launch
/// succeeded, and this is what `remuda watch` reports once the route is down.
/// Adjacent to `idle-api-error`, and distinct from it — retrying cannot clear a
/// route whose host is gone.
pub const API_ROUTE_DOWN: &str = "api-route-down";

/// `apiVia: "self"` — the Hub host itself as the proxy host; §4.4 (D-047).
///
/// The owner's case: the Mac runs the Hub and the relay, so this names it
/// without the operator having to look up its `hst_…`.
pub const API_VIA_SELF: &str = "self";
/// `apiVia: "none"` — force direct delivery for this one dispatch, overriding
/// a profile or project that would otherwise proxy; §4.4 (D-047).
pub const API_VIA_NONE: &str = "none";

/// The per-dispatch `apiVia` override; `protocol.md` §4.4 (D-047).
///
/// Stays a **string** on the wire, because two of its three values are
/// keywords and only one carries an id. The waterfall is
/// request `apiVia` > project > profile `delivery` > `direct`, and it is
/// resolved by the Hub after host placement, not here.
///
/// Serialized through its wire spelling rather than a derive, so the type is
/// ergonomic in Rust while the frame keeps the operator-facing keyword form
/// (`hst_…` / `self` / `none`) that the CLI and bot commands already speak.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(try_from = "String", into = "String")]
pub enum ApiViaOverride {
    /// A specific proxy host.
    Host(HostId),
    /// [`API_VIA_SELF`]: the Hub host.
    HubHost,
    /// [`API_VIA_NONE`]: no proxy, whatever the profile says.
    Direct,
}

impl TryFrom<String> for ApiViaOverride {
    type Error = WireValueError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<ApiViaOverride> for String {
    fn from(value: ApiViaOverride) -> Self {
        value.as_wire()
    }
}

impl ApiViaOverride {
    /// Wire spelling (`hst_…`, `self`, or `none`).
    #[must_use]
    pub fn as_wire(&self) -> String {
        match self {
            Self::Host(id) => id.as_id().as_str().to_owned(),
            Self::HubHost => API_VIA_SELF.to_owned(),
            Self::Direct => API_VIA_NONE.to_owned(),
        }
    }

    /// Parse the wire string. Unknown spellings are an error, never a default:
    /// a mistyped host must not quietly become `direct`, which would send the
    /// request — and the credential — to a machine the operator excluded.
    pub fn parse(value: &str) -> Result<Self, WireValueError> {
        match value.trim() {
            API_VIA_SELF => Ok(Self::HubHost),
            API_VIA_NONE => Ok(Self::Direct),
            other => HostId::from_str(other).map(Self::Host),
        }
    }
}

impl std::str::FromStr for ApiViaOverride {
    type Err = WireValueError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl std::fmt::Display for ApiViaOverride {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.as_wire())
    }
}

wire_enum!(SelectionReason, "4.1", {
    Pinned => "pinned",
    WeightedHealthy => "weighted-healthy",
    ExplicitRecovery => "explicit-recovery",
});

wire_enum!(InputDelivery, "4.1", {
    Stdio => "stdio",
    Tty => "tty",
    DeferredArgv => "deferred-argv",
});

wire_enum!(ApprovalAuthority, "4.1", {
    RuntimeHost => "runtime-host",
    RuntimeHook => "runtime-hook",
    NativeTty => "native-tty",
    Unknown => "unknown",
});

wire_enum!(Completeness, "5.1", {
    Structured => "structured",
    Partial => "partial",
    ScreenDerived => "screen-derived",
    Opaque => "opaque",
});

wire_enum!(SourceChannel, "5.1", {
    Stdout => "stdout",
    Stderr => "stderr",
    Transcript => "transcript",
    WorkflowJournal => "workflow-journal",
    Hook => "hook",
    Rpc => "rpc",
    Pty => "pty",
    Herdr => "herdr",
    Runtime => "runtime",
    File => "file",
    Osc => "osc",
    Screen => "screen",
});

wire_enum!(SourceDelivery, "5.1", {
    Live => "live",
    Replay => "replay",
    Unknown => "unknown",
});

wire_enum!(Redaction, "5.1", {
    None => "none",
    DerivedRedacted => "derived-redacted",
    Unavailable => "unavailable",
});

wire_enum!(MutationOperation, "5.2", {
    Open => "open",
    Append => "append",
    Replace => "replace",
    Close => "close",
});

wire_enum!(MessageRole, "5.2", {
    User => "user",
    Assistant => "assistant",
    System => "system",
});

wire_enum!(MessagePhase, "5.2", {
    Input => "input",
    Commentary => "commentary",
    Final => "final",
    Unknown => "unknown",
});

wire_enum!(MessageOrigin, "5.2", {
    Human => "human",
    InjectedSkill => "injected-skill",
    InjectedCommandOutput => "injected-command-output",
    HookContext => "hook-context",
    ToolResult => "tool-result",
    Compaction => "compaction",
    Unknown => "unknown",
});

wire_enum!(ContentStatus, "5.2", {
    Queued => "queued",
    Streaming => "streaming",
    Complete => "complete",
    Interrupted => "interrupted",
    Unknown => "unknown",
});

wire_enum!(ThoughtRepresentation, "5.2", {
    Summary => "summary",
    Text => "text",
    Redacted => "redacted",
});

wire_enum!(ToolCategory, "5.2", {
    Shell => "shell",
    FileRead => "file-read",
    FileWrite => "file-write",
    Search => "search",
    Mcp => "mcp",
    Workflow => "workflow",
    Agent => "agent",
    Other => "other",
});

wire_enum!(ToolCallState, "5.2", {
    Proposed => "proposed",
    Running => "running",
    Unknown => "unknown",
});

wire_enum!(ResultStage, "5.2", {
    Partial => "partial",
    Final => "final",
});

wire_enum!(ToolOutcome, "5.2", {
    Succeeded => "succeeded",
    Failed => "failed",
    Denied => "denied",
    Cancelled => "cancelled",
    Unknown => "unknown",
});

wire_enum!(ChangeApplication, "5.2", {
    Proposed => "proposed",
    Applied => "applied",
    Unknown => "unknown",
});

wire_enum!(WorkflowEngine, "5.3", {
    ClaudeWorkflow => "claude-workflow",
});

wire_enum!(WorkflowState, "5.3", {
    Queued => "queued",
    Running => "running",
    Completed => "completed",
    Failed => "failed",
    Cancelled => "cancelled",
    Unknown => "unknown",
});

wire_enum!(DecisionEffect, "5.4", {
    AllowOnce => "allow-once",
    AllowSession => "allow-session",
    Deny => "deny",
    Cancel => "cancel",
    NativeSpecific => "native-specific",
});

wire_enum!(QuestionInput, "5.4", {
    Text => "text",
    SingleSelect => "single-select",
    MultiSelect => "multi-select",
});

wire_enum!(ElicitationMode, "5.4", {
    Form => "form",
    Url => "url",
    NativeExtension => "native-extension",
});

wire_enum!(ElicitationAction, "5.4", {
    Accept => "accept",
    Decline => "decline",
    Cancel => "cancel",
});

wire_enum!(InteractionExpiredReason, "5.4", {
    Deadline => "deadline",
    NativeCancelled => "native-cancelled",
    GenerationEnded => "generation-ended",
    Replaced => "replaced",
    ChannelLost => "channel-lost",
});

wire_enum!(LifecycleTopic, "5.5", {
    Session => "session",
    Turn => "turn",
    Hook => "hook",
    Subagent => "subagent",
    Task => "task",
    Plan => "plan",
    Configuration => "configuration",
    Permission => "permission",
    Diagnostic => "diagnostic",
    Reconciliation => "reconciliation",
});

wire_enum!(Severity, "5.5", {
    Info => "info",
    Warning => "warning",
    Error => "error",
});

wire_enum!(UsageScope, "5.5", {
    Message => "message",
    Turn => "turn",
    Session => "session",
    WorkflowMember => "workflow-member",
});

wire_enum!(UsageMode, "5.5", {
    Snapshot => "snapshot",
    Delta => "delta",
});

wire_enum!(InputAccounting, "5.5", {
    TotalIncludingCache => "total-including-cache",
    Uncached => "uncached",
    ProviderSpecific => "provider-specific",
    Unknown => "unknown",
});

wire_enum!(Accounting, "5.5", {
    Reported => "reported",
    Estimated => "estimated",
});

wire_enum!(ArtifactAction, "5.5", {
    Declared => "declared",
    Available => "available",
    Updated => "updated",
    Removed => "removed",
});

wire_enum!(ArtifactType, "5.5", {
    File => "file",
    Image => "image",
    Html => "html",
    Url => "url",
    Native => "native",
    Diff => "diff",
});

wire_enum!(ArtifactVerification, "5.5", {
    Declared => "declared",
    ReadVerified => "read-verified",
    NativeConfirmed => "native-confirmed",
});

wire_enum!(TtyRepresentation, "5.5", {
    PtyBytes => "pty-bytes",
    RenderedAnsi => "rendered-ansi",
});

wire_enum!(TtyInputDelivery, "5.5", {
    Written => "written",
    Unknown => "unknown",
});

wire_enum!(OpaqueReason, "5.5", {
    UnknownType => "unknown-type",
    UnknownVersion => "unknown-version",
    Malformed => "malformed",
    UnsupportedExtension => "unsupported-extension",
    UnmappedFields => "unmapped-fields",
    ScreenSnapshot => "screen-snapshot",
});

wire_enum!(OpaqueImpact, "5.5", {
    Presentation => "presentation",
    Control => "control",
    Terminal => "terminal",
});

wire_enum!(RetryAction, "9.1", {
    Never => "never",
    ReadOnly => "read-only",
    SameCommandQuery => "same-command-query",
    AfterReconciliation => "after-reconciliation",
    NewCommand => "new-command",
});

wire_enum!(ExecutionState, "9.1", {
    NotDispatched => "not-dispatched",
    PossiblyDispatched => "possibly-dispatched",
    Accepted => "accepted",
    Settled => "settled",
    NotApplicable => "not-applicable",
});

wire_enum!(JsonRpcVersion, "7.1", {
    V2 => "2.0",
});

wire_enum!(SnapshotMode, "7.3", {
    Required => "required",
    IfNeeded => "if-needed",
    None => "none",
});

wire_enum!(WaitReason, "7.2", {
    ConditionMet => "condition-met",
    Timeout => "timeout",
    Unknown => "unknown",
});

wire_enum!(RunWaitCondition, "7.2", {
    Terminal => "terminal",
    Interaction => "interaction",
    ObservedUpdate => "observed-update",
});

wire_enum!(TtyMode, "7.4", {
    Read => "read",
    Write => "write",
});

wire_enum!(CloseMode, "7.2", {
    Terminate => "terminate",
});

wire_enum!(ForkBoundaryType, "7.2", {
    LatestTerminal => "latest-terminal",
});

wire_enum!(ObjectPurpose, "7.2", {
    Input => "input",
    Settings => "settings",
    Answer => "answer",
});

wire_enum!(RegistryKind, "5.5", {
    Lifecycle => "lifecycle",
});

wire_enum!(EnvVisibility, "4.1", {
    Private => "private",
});

wire_enum!(ObservationKind, "5.1", {
    Message => "message",
    Thought => "thought",
    ToolCall => "tool_call",
    ToolResult => "tool_result",
    InteractionRequested => "interaction.requested",
    InteractionAnswered => "interaction.answered",
    InteractionExpired => "interaction.expired",
    WorkflowRun => "workflow.run",
    WorkflowPhase => "workflow.phase",
    WorkflowMember => "workflow.member",
    Lifecycle => "lifecycle",
    Usage => "usage",
    Artifact => "artifact",
    Effort => "effort",
    Model => "model",
    Permission => "permission",
    RawTty => "raw_tty",
    Opaque => "opaque",
});

wire_enum!(ErrorCode, "9.1", {
    Unauthenticated => "UNAUTHENTICATED",
    ScopeDenied => "SCOPE_DENIED",
    HostOffline => "HOST_OFFLINE",
    OwnerFenced => "OWNER_FENCED",
    ProtocolVersionUnsupported => "PROTOCOL_VERSION_UNSUPPORTED",
    SchemaVersionUnsupported => "SCHEMA_VERSION_UNSUPPORTED",
    CapabilityUnsupported => "CAPABILITY_UNSUPPORTED",
    CapabilityUnknown => "CAPABILITY_UNKNOWN",
    NativeFeatureDisabled => "NATIVE_FEATURE_DISABLED",
    BinaryChanged => "BINARY_CHANGED",
    InvalidLaunchSpec => "INVALID_LAUNCH_SPEC",
    SettingsIsolationUnavailable => "SETTINGS_ISOLATION_UNAVAILABLE",
    ProviderProtocolMismatch => "PROVIDER_PROTOCOL_MISMATCH",
    ProviderUnavailable => "PROVIDER_UNAVAILABLE",
    CredentialUnavailable => "CREDENTIAL_UNAVAILABLE",
    WorkspaceNotFound => "WORKSPACE_NOT_FOUND",
    WorkspaceBusy => "WORKSPACE_BUSY",
    WorktreeBusy => "WORKTREE_BUSY",
    WorktreeDirty => "WORKTREE_DIRTY",
    NativeSessionNotFound => "NATIVE_SESSION_NOT_FOUND",
    NativeSessionOwned => "NATIVE_SESSION_OWNED",
    NativeGenerationMismatch => "NATIVE_GENERATION_MISMATCH",
    AttachWouldWake => "ATTACH_WOULD_WAKE",
    ControlUnavailable => "CONTROL_UNAVAILABLE",
    CommandIdConflict => "COMMAND_ID_CONFLICT",
    CommandExpired => "COMMAND_EXPIRED",
    CommandOutcomeUnknown => "COMMAND_OUTCOME_UNKNOWN",
    RunNotActive => "RUN_NOT_ACTIVE",
    InteractionAlreadyAnswered => "INTERACTION_ALREADY_ANSWERED",
    InteractionStale => "INTERACTION_STALE",
    InteractionExpired => "INTERACTION_EXPIRED",
    InteractionSchemaUnsupported => "INTERACTION_SCHEMA_UNSUPPORTED",
    InteractionNotAnswerable => "INTERACTION_NOT_ANSWERABLE",
    InvalidAnswer => "INVALID_ANSWER",
    NativeResponseUnknown => "NATIVE_RESPONSE_UNKNOWN",
    CursorExpired => "CURSOR_EXPIRED",
    JournalGap => "JOURNAL_GAP",
    JournalDiverged => "JOURNAL_DIVERGED",
    JournalUnavailable => "JOURNAL_UNAVAILABLE",
    StateUnknown => "STATE_UNKNOWN",
    ResourceLimit => "RESOURCE_LIMIT",
    TtyLeaseLost => "TTY_LEASE_LOST",
    TtyHistoryGap => "TTY_HISTORY_GAP",
    ObjectNotFound => "OBJECT_NOT_FOUND",
    ObjectRevisionMismatch => "OBJECT_REVISION_MISMATCH",
    NativeProtocolError => "NATIVE_PROTOCOL_ERROR",
    WaitTimeout => "WAIT_TIMEOUT",
});

wire_enum!(MethodName, "7.2", {
    RuntimeHello => "runtime.hello",
    RuntimeHeartbeat => "runtime.heartbeat",
    HostReport => "host.report",
    HostGet => "host.get",
    HostList => "host.list",
    DriverList => "driver.list",
    DriverCapabilities => "driver.capabilities",
    WorkspaceRegister => "workspace.register",
    WorkspaceGet => "workspace.get",
    WorkspaceList => "workspace.list",
    WorktreeCreate => "worktree.create",
    WorktreeRemove => "worktree.remove",
    InstanceCreate => "instance.create",
    InstanceAttach => "instance.attach",
    InstanceOpenTerminal => "instance.open_terminal",
    InstanceResume => "instance.resume",
    InstanceSend => "instance.send",
    InstanceConfigure => "instance.configure",
    InstanceFork => "instance.fork",
    InstanceCancel => "instance.cancel",
    InstanceClose => "instance.close",
    InstanceGet => "instance.get",
    InstanceList => "instance.list",
    CommandGet => "command.get",
    CommandList => "command.list",
    RunGet => "run.get",
    RunList => "run.list",
    RunWait => "run.wait",
    WorkflowWait => "workflow.wait",
    InteractionList => "interaction.list",
    InteractionGet => "interaction.get",
    InteractionRespond => "interaction.respond",
    EventsSubscribe => "events.subscribe",
    EventsRead => "events.read",
    EventsAck => "events.ack",
    EventsUnsubscribe => "events.unsubscribe",
    ReconcileInstance => "reconcile.instance",
    TtyAttach => "tty.attach",
    TtyDetach => "tty.detach",
    TtyWrite => "tty.write",
    TtyResize => "tty.resize",
    ObjectStat => "object.stat",
    ObjectRead => "object.read",
    ObjectPrepare => "object.prepare",
    ObjectWrite => "object.write",
    ObjectCommit => "object.commit",
});

wire_enum!(NotificationName, "7.3", {
    EventsBatch => "events.batch",
});

wire_enum!(HerdrRepresentation, "1.3", {
    RenderedAnsi => "rendered-ansi",
});

wire_enum!(BgInputDelivery, "4.1", {
    DeferredArgv => "deferred-argv",
});

wire_enum!(ArgvInputPolicy, "4.1", {
    ExplicitNonSecret => "explicit-non-secret",
});

wire_enum!(AdapterTransport, "3.2", {
    NativeRustWire => "native-rust-wire",
    ClaudeSdkSidecar => "claude-sdk-sidecar",
    ClaudePtyHerdr => "claude-pty-herdr",
    ClaudeBgHerdrAttach => "claude-bg-herdr-attach",
    CodexAppserverSpawn => "codex-appserver-spawn",
    CodexEmbedded => "codex-embedded",
    GrokAcp => "grok-acp",
    AgyNative => "agy-native",
    GenericHerdr => "generic-herdr",
    ShellPty => "shell-pty",
});
