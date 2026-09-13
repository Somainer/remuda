//! Enums wire declarations; `protocol.md`.

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

wire_enum!(EffortName, "4.1", {
    Low => "low",
    Medium => "medium",
    High => "high",
    Xhigh => "xhigh",
    Max => "max",
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
