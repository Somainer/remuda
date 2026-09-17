//! Reproducible schema and TypeScript generation from the wire declarations; `protocol.md` §12.

use crate::*;
use schemars::generate::SchemaSettings;
use serde_json::{Map, Value, json};

/// A schema construct without a reviewed TypeScript rendering; `protocol.md` §12.
#[derive(Debug, thiserror::Error)]
#[error("unsupported generated schema: {0}")]
pub struct SchemaExportError(pub String);

/// Generate the v1 message schema and definitions for every exported wire type; §12.
///
/// Definitions use serialization semantics: required nullable fields remain required,
/// and only fields with explicit omission rules become optional in TypeScript.
pub fn schema_document() -> Value {
    let mut generator = SchemaSettings::draft2020_12()
        .for_serialize()
        .into_generator();
    macro_rules! register {
        ($($ty:ty),* $(,)?) => { $(let _ = generator.subschema_for::<$ty>();)* };
    }
    // ROOT_TYPES: additional nested types are discovered by schemars.
    register! {
        AdapterTransport,
        Acceptance,
        AcceptanceScope,
        Accounting,
        AcpRef,
        Activity,
        ActorRef,
        ActorType,
        AgentKind,
        AgyPermission,
        AgyPermissionMode,
        AgyRef,
        ApprovalAnswer,
        ApprovalAuthority,
        ApprovalPolicy,
        ApprovalRequest,
        ApprovalsReviewer,
        ArgvInputPolicy,
        ArtifactAction,
        ArtifactLocator,
        ArtifactPayload,
        ArtifactType,
        ArtifactVerification,
        AttachMode,
        AttachRef,
        BgInputDelivery,
        BinaryChannel,
        BinaryHeader,
        BlobLocator,
        BoolLiteral<false>,
        BoolLiteral<true>,
        Capability,
        CapabilityEvidence,
        CapabilityName,
        CapabilityProvision,
        CapabilitySet,
        CapabilitySnapshot,
        CapabilityState,
        CarrierSpec,
        ChangeApplication,
        ClaudeBgCarrier,
        ClaudeBgRef,
        ClaudeInteractionMode,
        ClaudePermission,
        ClaudePermissionMode,
        ClaudeRef,
        CloseMode,
        CodexExecution,
        CodexPermission,
        CodexRef,
        Command,
        CommandAuthority,
        CommandEnvelope<Value>,
        CommandId,
        CommandListParams,
        CommandOperation,
        CommandOrigin,
        CommandParams,
        CommandResult,
        CommandState,
        CommandTarget,
        CommittedAnswer,
        Completeness,
        CompletionScope,
        ConnectionLease,
        Connectivity,
        ContentBlock,
        ContentStatus,
        ContextNeed,
        ConversationNode,
        Cost,
        CreateInitialInput,
        CreateWorktree,
        CredentialEnv,
        DeadlineSource,
        DecisionEffect,
        DecisionOption,
        DeliveryState,
        Digest,
        DiffScopeCheck,
        DispatchState,
        DriverCapabilitiesParams,
        DriverDescriptor,
        DriverInput,
        DriverKind,
        EffortEffective,
        EffortName,
        EffortPayload,
        EffortSelection,
        EffortSource,
        ObservedEffort,
        PermissionEffective,
        PermissionPayload,
        PermissionSource,
        TuiMode,
        ElicitationAction,
        ElicitationAnswer,
        ElicitationMode,
        ElicitationRequest,
        EmptyResult,
        EntityLifecycle,
        EntityMeta<Id>,
        EnvBinding,
        EnvVisibility,
        ErrorCode,
        ErrorDetails,
        EventId,
        EventsAckParams,
        EventsAckResult,
        EventsBatch,
        EventsReadParams,
        EventsReadResult,
        EventsSubscribeParams,
        EventsSubscribeResult,
        EvidenceType,
        ExecutionState,
        ExecutorRef,
        ExistingWorktree,
        ExpectedState,
        FileChange,
        FileCursor,
        ForkBoundary,
        ForkBoundaryType,
        ForwardIntent,
        GateCancelParams,
        GateEventKind,
        GateEventParams,
        GateJob,
        GateJobId,
        GateJobState,
        GateMode,
        GateRunLog,
        GateRunParams,
        GateRunResult,
        GateStep,
        GateThenParams,
        GateThenResult,
        GateWebMode,
        GenericPermission,
        GenericPermissionMode,
        GrantVerb,
        GrokPermission,
        GrokPermissionMode,
        HeartbeatParams,
        HeartbeatResult,
        HelloParams,
        HelloResult,
        HerdrRef,
        HerdrRepresentation,
        HerdrServer,
        HistoryCoverage,
        HookCursor,
        Host,
        HostEnv,
        HostId,
        HostParams,
        HostReportParams,
        HostReportResult,
        HostState,
        HostTransport,
        HostTransportMode,
        Id,
        InputAccounting,
        InputDelivery,
        InputOrigin,
        Instance,
        InstanceAttachParams,
        InstanceCancelParams,
        InstanceCloseParams,
        InstanceConfigureParams,
        InstanceCreateParams,
        InstanceCreateResult,
        InstanceForkParams,
        InstanceId,
        InstanceLifecycle,
        InstanceListParams,
        InstanceOpenTerminalParams,
        InstanceParams,
        InstanceParent,
        InstanceResumeParams,
        InstanceScope,
        InstanceSendParams,
        InstanceSnapshot,
        InstanceSpec,
        Interaction,
        InteractionAnswer,
        InteractionAnsweredPayload,
        InteractionCarrier,
        InteractionExpiredPayload,
        InteractionExpiredReason,
        InteractionId,
        InteractionKind,
        InteractionListParams,
        InteractionParams,
        InteractionRequest,
        InteractionRequestKey,
        InteractionRequestedPayload,
        InteractionResolution,
        InteractionResolutionReason,
        InteractionRespondParams,
        InteractionRespondResult,
        InteractionState,
        JournalEvent,
        JournalWatermark,
        JsonRpcVersion,
        Knowledge<Value>,
        LaunchedBy,
        LifecycleEntity,
        LifecyclePayload,
        LifecycleTopic,
        LiteralEnv,
        Mandate,
        MandateLink,
        MediaBlock,
        MessagePayload,
        MessageOrigin,
        MessagePhase,
        MessageRole,
        MethodCall,
        MethodName,
        ModelClass,
        ModelEffective,
        EffectiveModel,
        ModelCatalogInfo,
        ModelListSource,
        ModelPayload,
        ObservedModel,
        ModelRoles,
        ModelSwitchInput,
        MutationOperation,
        NamedPermissions,
        NativeHome,
        NativeHomeMode,
        NativeLifecycle,
        NativeLocator,
        NativeRef,
        NativeRequestKey,
        NativeRequestValueType,
        NativeTerminalFrame,
        NativeTurn,
        NodeMutation,
        NodeReceipt,
        NonEmpty<Value>,
        NotificationBody,
        NotificationName,
        ObservedRateLimits,
        ObjectCommitParams,
        ObjectCommitResult,
        ObjectMetadata,
        ObjectParams,
        ObjectPrepareParams,
        ObjectPrepareResult,
        ObjectPurpose,
        ObjectReadParams,
        ObjectReadResult,
        ObjectWriteParams,
        ObjectWriteResult,
        Observation,
        ObservationKind,
        ObservationPayload,
        ObservationSource,
        OpaqueBlock,
        OpaqueImpact,
        OpaquePayload,
        OpaqueReason,
        OutstandingWork,
        Ownership,
        Page<Value>,
        PageParams,
        Parentage,
        PathStyle,
        PermissionMode,
        PlanReviewAnswer,
        PlanReviewRequest,
        PlacementLedgerRow,
        PlacementRejection,
        Platform,
        ProcessExit,
        ProcessIdentity,
        ProcessRef,
        ProfileRef,
        Project,
        ProjectConfigurablePolicy,
        ProjectEnforcedPolicy,
        ProjectGate,
        ProjectGateLane,
        ProjectHostQuota,
        ProjectId,
        ProjectLaunchDefaults,
        ProjectMember,
        ProjectPlacement,
        ProjectPolicy,
        ProjectProviderRef,
        PromptInput,
        PromptMode,
        ProtocolRange,
        ProtocolVersion,
        ProviderIngress,
        ProviderOverlaySpec,
        ProviderProfile,
        ProviderProfileKind,
        ProviderSecretView,
        ProviderSelection,
        PtyBackend,
        PtyCarrier,
        QuestionAnswer,
        QuestionField,
        QuestionFieldAnswer,
        QuestionInput,
        QuestionOption,
        QuestionRequest,
        RateLimitWindow,
        RawRef,
        RawTtyPayload,
        ReconcileInstanceParams,
        ReconcileInstanceResult,
        Redaction,
        RegistryEvent,
        RegistryKind,
        RegistrySnapshot,
        RepositoryRef,
        ResolutionState,
        ResourceBlock,
        ResultStage,
        ResumeCursor,
        RetryAction,
        RpcError,
        RpcErrorData,
        RpcFailure,
        RpcNotification,
        RpcRequest,
        RpcResponse<Value>,
        RpcSuccess<Value>,
        Run,
        RunCause,
        RunCauseType,
        RunId,
        RunListParams,
        RunParams,
        RunResult,
        RunState,
        RunWaitCondition,
        RunWaitParams,
        RunWaitResult,
        RuntimeCapability,
        RuntimeCursor,
        RuntimeError,
        SandboxExecution,
        SandboxMode,
        SchemaVersion,
        SelectionReason,
        SendInput,
        Sensitivity,
        SettingsFormat,
        SettingsOverlay,
        Settlement,
        SettlementOutcome,
        Severity,
        SignalTier,
        Snapshot,
        SnapshotMode,
        SourceChannel,
        SourceCursor,
        SourceDelivery,
        SpendControl,
        StateConfidence,
        SteerInput,
        StreamCursor,
        StreamUuid,
        SubscriptionParams,
        SupplyConcurrency,
        SupplyCredits,
        SupplyProfile,
        SupplyReserve,
        SupplyState,
        Task,
        TaskBudget,
        TaskClass,
        TaskDep,
        TaskId,
        TaskPin,
        TaskPlacementRef,
        TaskSpec,
        TaskState,
        TaskNotification,
        TerminalEvidence,
        TextBlock,
        ThoughtPayload,
        ThoughtRepresentation,
        Timestamp,
        ToolCallPayload,
        ToolCallState,
        ToolCategory,
        ToolOutcome,
        ToolResultPayload,
        TranscriptRef,
        TransportLimits,
        TtyAttachParams,
        TtyAttachResult,
        TtyCursor,
        TtyDetachParams,
        TtyInput,
        TtyInputDelivery,
        TtyMode,
        TtyOutput,
        TtyRepresentation,
        TtyResize,
        TtyResizeParams,
        TtyResizeResult,
        TtyWriteParams,
        TtyWriterLease,
        U64,
        UrlLocator,
        UsageMode,
        UsagePayload,
        UsageScope,
        WaitReason,
        WindowSource,
        WorkerProvisionParams,
        WorkerProvisionResult,
        WorkerRemoveParams,
        WorkerRemoveResult,
        WorkerRoster,
        WorkerRosterId,
        WorkerState,
        WorkerWatch,
        WorkerWatchStatus,
        WorkflowEngine,
        WorkflowMemberPayload,
        WorkflowPhasePayload,
        WorkflowRunPayload,
        WorkflowState,
        WorkflowWaitParams,
        WorkflowWaitResult,
        Workspace,
        WorkspaceFileLocator,
        WorkspaceId,
        WorkspaceListParams,
        WorkspaceParams,
        WorkspaceRegisterParams,
        WorkspaceState,
        WorktreeCreateParams,
        WorktreeId,
        WorktreeOwner,
        WorktreeRecord,
        WorktreeRemoveParams,
        WorktreeSpec,
        WorktreeState,
        WritePolicy,
        WriterLease,
    }
    let request = generator.subschema_for::<RpcRequest>();
    let response = generator.subschema_for::<RpcResponse<Value>>();
    let notification = generator.subschema_for::<RpcNotification>();
    canonical(json!({
        "$schema":"https://json-schema.org/draft/2020-12/schema",
        "$comment":"Generated from remuda-protocol; run just gen-types. The root is a Hub/Node message; other wire types are in $defs.",
        "title":"RemudaProtocol",
        "anyOf":[request, response, notification],
        "$defs":generator.take_definitions(true)
    }))
}

/// Render TypeScript declarations from this crate's schema vocabulary; `protocol.md` §12.
///
/// New schema keywords fail generation until reviewed. TypeScript expresses data types;
/// numeric/string bounds, formats, and cross-field authorization still require validation.
pub fn typescript(document: &Value) -> Result<String, SchemaExportError> {
    let document = canonical(document.clone());
    let definitions = document
        .get("$defs")
        .and_then(Value::as_object)
        .ok_or_else(|| SchemaExportError("missing $defs".into()))?;
    check_references(&document, definitions)?;
    let mut output = String::from(
        "// Generated from remuda-protocol by just gen-types. Do not edit.\n// String formats and numeric bounds are checked by the JSON Schema and Rust ingress.\n\n",
    );
    output.push_str(&format!("export const PROTOCOL_VERSION = {{ major: {}, minor: {} }} as const;\nexport const BINARY_HEADER_LEN = {};\n\n", PROTOCOL_VERSION.major, PROTOCOL_VERSION.minor, BINARY_HEADER_LEN));
    output.push_str(&format!(
        "export const M0_REQUIRED_ERROR_CODES = {} as const;\n\n",
        serde_json::to_value(M0_REQUIRED_ERROR_CODES)
            .map_err(|error| SchemaExportError(error.to_string()))?
    ));
    for (name, schema) in definitions {
        if !is_identifier(name) {
            return Err(SchemaExportError(format!("invalid type name {name}")));
        }
        if let Some(description) = schema.get("description").and_then(Value::as_str) {
            output.push_str(&format!(
                "/** {} */\n",
                description.replace("*/", "* /").replace('\n', " ")
            ));
        }
        output.push_str(&format!("export type {name} = {};\n\n", ts_type(schema)?));
    }
    Ok(output.trim_end().to_owned() + "\n")
}

fn canonical(value: Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut fields: Vec<_> = object.into_iter().collect();
            fields.sort_by(|(left, _), (right, _)| left.cmp(right));
            Value::Object(
                fields
                    .into_iter()
                    .map(|(key, value)| (key, canonical(value)))
                    .collect(),
            )
        }
        Value::Array(values) => Value::Array(values.into_iter().map(canonical).collect()),
        value => value,
    }
}

fn check_references(
    value: &Value,
    definitions: &Map<String, Value>,
) -> Result<(), SchemaExportError> {
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                if key == "$ref" {
                    let name = child
                        .as_str()
                        .and_then(|value| value.strip_prefix("#/$defs/"))
                        .filter(|name| definitions.contains_key(*name));
                    if name.is_none() {
                        return Err(SchemaExportError(format!("unresolved reference {child}")));
                    }
                }
                check_references(child, definitions)?;
            }
        }
        Value::Array(values) => {
            for child in values {
                check_references(child, definitions)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn is_identifier(name: &str) -> bool {
    !name.is_empty()
        && name.chars().enumerate().all(|(i, c)| {
            c == '_' || c == '$' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit())
        })
}

fn ts_type(schema: &Value) -> Result<String, SchemaExportError> {
    if schema == &Value::Bool(true) {
        return Ok("unknown".into());
    }
    if schema == &Value::Bool(false) {
        return Ok("never".into());
    }
    let object = schema
        .as_object()
        .ok_or_else(|| SchemaExportError("non-object schema".into()))?;
    const KEYWORDS: &[&str] = &[
        "$ref",
        "$defs",
        "$schema",
        "$comment",
        "title",
        "description",
        "default",
        "examples",
        "deprecated",
        "readOnly",
        "writeOnly",
        "type",
        "const",
        "enum",
        "oneOf",
        "anyOf",
        "allOf",
        "not",
        "properties",
        "required",
        "additionalProperties",
        "items",
        "prefixItems",
        "minItems",
        "maxItems",
        "uniqueItems",
        "minLength",
        "maxLength",
        "pattern",
        "format",
        "minimum",
        "maximum",
        "exclusiveMinimum",
        "exclusiveMaximum",
        "multipleOf",
    ];
    for key in object.keys() {
        if !KEYWORDS.contains(&key.as_str()) {
            return Err(SchemaExportError(format!("keyword {key}")));
        }
    }
    let mut terms = Vec::new();
    if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
        let name = reference
            .strip_prefix("#/$defs/")
            .filter(|name| is_identifier(name))
            .ok_or_else(|| SchemaExportError(format!("nonlocal reference {reference}")))?;
        terms.push(name.to_owned());
    }
    if let Some(constant) = object.get("const") {
        terms.push(literal(constant)?);
    }
    if let Some(values) = object.get("enum") {
        terms.push(format!(
            "({})",
            array(values)?
                .iter()
                .map(literal)
                .collect::<Result<Vec<_>, _>>()?
                .join(" | ")
        ));
    }
    for (keyword, separator) in [("oneOf", " | "), ("anyOf", " | "), ("allOf", " & ")] {
        if let Some(values) = object.get(keyword) {
            terms.push(format!(
                "({})",
                array(values)?
                    .iter()
                    .map(ts_type)
                    .collect::<Result<Vec<_>, _>>()?
                    .join(separator)
            ));
        }
    }
    if let Some(negated) = object.get("not") {
        let map = negated
            .as_object()
            .ok_or_else(|| SchemaExportError("not requires an object".into()))?;
        if map.len() != 1 || !map.contains_key("required") {
            return Err(SchemaExportError("unsupported negation".into()));
        }
        let forbidden = array(&map["required"])?;
        if forbidden.len() != 1 {
            return Err(SchemaExportError("multiple negated required keys".into()));
        }
        terms.push(format!("{{ {}?: never }}", literal(&forbidden[0])?));
    }
    // A const or enum already expresses the primitive type more narrowly.
    if !object.contains_key("const") && !object.contains_key("enum") {
        if let Some(kind) = object.get("type") {
            let types = match kind {
                Value::Array(values) => values.clone(),
                Value::String(_) => vec![kind.clone()],
                _ => return Err(SchemaExportError("invalid type".into())),
            };
            let values = types
                .iter()
                .map(|kind| match kind.as_str() {
                    Some("null") => Ok("null".into()),
                    Some("boolean") => Ok("boolean".into()),
                    Some("string") => Ok("string".into()),
                    Some("integer" | "number") => Ok("number".into()),
                    Some("object") => ts_object(object),
                    Some("array") => ts_array(object),
                    _ => Err(SchemaExportError("unknown primitive".into())),
                })
                .collect::<Result<Vec<String>, _>>()?;
            terms.push(format!("({})", values.join(" | ")));
        } else if object.contains_key("properties") {
            terms.push(ts_object(object)?);
        }
    }
    Ok(if terms.is_empty() {
        "unknown".into()
    } else {
        terms.join(" & ")
    })
}

fn literal(value: &Value) -> Result<String, SchemaExportError> {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => Ok(value.to_string()),
        _ => Err(SchemaExportError("non-scalar literal".into())),
    }
}

fn array(value: &Value) -> Result<&Vec<Value>, SchemaExportError> {
    value
        .as_array()
        .ok_or_else(|| SchemaExportError("expected schema array".into()))
}

fn ts_object(object: &Map<String, Value>) -> Result<String, SchemaExportError> {
    let mut fields = Vec::new();
    let required = object.get("required").map(array).transpose()?;
    if let Some(properties) = object.get("properties") {
        for (name, schema) in properties
            .as_object()
            .ok_or_else(|| SchemaExportError("invalid properties".into()))?
        {
            let optional =
                if required.is_some_and(|keys| keys.contains(&Value::String(name.clone()))) {
                    ""
                } else {
                    "?"
                };
            fields.push(format!(
                "  {}{optional}: {};",
                Value::String(name.clone()),
                ts_type(schema)?
            ));
        }
    }
    if let Some(additional) = object.get("additionalProperties") {
        if additional != &Value::Bool(false) {
            fields.push(format!("  [key: string]: {};", ts_type(additional)?));
        }
    } else {
        fields.push("  [key: string]: unknown;".into());
    }
    Ok(format!("{{\n{}\n}}", fields.join("\n")))
}

fn ts_array(object: &Map<String, Value>) -> Result<String, SchemaExportError> {
    let item = object
        .get("items")
        .map(ts_type)
        .transpose()?
        .unwrap_or_else(|| "unknown".into());
    if let Some(prefix) = object.get("prefixItems") {
        let mut values = array(prefix)?
            .iter()
            .map(ts_type)
            .collect::<Result<Vec<_>, _>>()?;
        if item != "never" {
            values.push(format!("...({item})[]"));
        }
        return Ok(format!("[{}]", values.join(", ")));
    }
    if object.get("minItems").and_then(Value::as_u64) == Some(1) {
        return Ok(format!("[({item}), ...({item})[]]"));
    }
    Ok(format!("({item})[]"))
}
