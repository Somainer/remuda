// Generated from remuda-protocol by just gen-types. Do not edit.
// String formats and numeric bounds are checked by the JSON Schema and Rust ingress.

export const PROTOCOL_VERSION = { major: 1, minor: 0 } as const;
export const BINARY_HEADER_LEN = 32;

export const M0_REQUIRED_ERROR_CODES = ["UNAUTHENTICATED","SCOPE_DENIED","HOST_OFFLINE","OWNER_FENCED","PROTOCOL_VERSION_UNSUPPORTED","SCHEMA_VERSION_UNSUPPORTED","CAPABILITY_UNSUPPORTED","CAPABILITY_UNKNOWN","BINARY_CHANGED","INVALID_LAUNCH_SPEC","NATIVE_GENERATION_MISMATCH","ATTACH_WOULD_WAKE","CONTROL_UNAVAILABLE","COMMAND_ID_CONFLICT","COMMAND_EXPIRED","COMMAND_OUTCOME_UNKNOWN","NATIVE_RESPONSE_UNKNOWN","JOURNAL_GAP","JOURNAL_DIVERGED","JOURNAL_UNAVAILABLE","STATE_UNKNOWN","RESOURCE_LIMIT","TTY_LEASE_LOST","TTY_HISTORY_GAP"] as const;

/** Acceptance; `protocol.md` §2.5. */
export type Acceptance = ({
  "eventIds": ((EventId)[]);
  "scope": AcceptanceScope;
  [key: string]: unknown;
});

/** AcceptanceScope wire values; `protocol.md` §2.5. */
export type AcceptanceScope = ("native-input" | "native-control" | "runtime-resource" | "tty-bytes");

/** Accounting wire values; `protocol.md` §5.5. */
export type Accounting = ("reported" | "estimated");

/** AcpRef; `protocol.md` §1.3. */
export type AcpRef = ({
  "protocolVersion": (number);
  "sessionId": (string);
  [key: string]: unknown;
});

/** Activity wire values; `protocol.md` §2.3. */
export type Activity = ("idle" | "working" | "waiting-interaction" | "draining");

/** ActorRef; `protocol.md` §1.1. */
export type ActorRef = ({
  "deviceId": (Id | (null));
  "instanceId": (InstanceId | (null));
  "principalId": Id;
  "type": ActorType;
  [key: string]: unknown;
});

/** ActorType wire values; `protocol.md` §1.1. */
export type ActorType = ("human" | "bot" | "agent" | "system");

/** AdapterTransport wire values; `protocol.md` §3.2. */
export type AdapterTransport = ("native-rust-wire" | "claude-sdk-sidecar" | "claude-pty-herdr" | "claude-bg-herdr-attach" | "codex-appserver-spawn" | "codex-embedded" | "grok-acp" | "agy-native" | "generic-herdr" | "shell-pty");

/** AgentKind wire values; `protocol.md` §1.3. */
export type AgentKind = ("claude" | "codex" | "grok" | "agy" | "generic" | "terminal");

/** AgyPermission; `protocol.md` §4.1. */
export type AgyPermission = ({
  "mode": AgyPermissionMode;
  [key: string]: unknown;
});

/** AgyPermissionMode wire values; `protocol.md` §4.1. */
export type AgyPermissionMode = ("native" | "accept-edits" | "plan" | "always-proceed");

/** AgyRef; `protocol.md` §1.3. */
export type AgyRef = ({
  "conversationId": (string);
  [key: string]: unknown;
});

/** ApprovalAnswer; `protocol.md` §5.4. */
export type ApprovalAnswer = ({
  "inputDigest": Digest;
  "optionId": (string);
  [key: string]: unknown;
});

/** ApprovalAuthority wire values; `protocol.md` §4.1. */
export type ApprovalAuthority = ("runtime-host" | "runtime-hook" | "native-tty" | "unknown");

/** ApprovalPolicy wire values; `protocol.md` §4.1. */
export type ApprovalPolicy = ("untrusted" | "on-request" | "never");

/** ApprovalRequest; `protocol.md` §5.4. */
export type ApprovalRequest = ({
  "actionRef": Id;
  "description": (string);
  "inputDigest": Digest;
  "options": ((DecisionOption)[]);
  "requestedPermissionsRef": (Id | (null));
  "title": (string);
  "toolCallId": (Id | (null));
  [key: string]: unknown;
});

/** ApprovalsReviewer wire values; `protocol.md` §4.1. */
export type ApprovalsReviewer = ("user");

/** ArgvInputPolicy wire values; `protocol.md` §4.1. */
export type ArgvInputPolicy = ("explicit-non-secret");

/** ArtifactAction wire values; `protocol.md` §5.5. */
export type ArtifactAction = ("declared" | "available" | "updated" | "removed");

/** ArtifactLocator; `protocol.md` §5.5. */
export type ArtifactLocator = (BlobLocator & ({
  "type": "blob";
  [key: string]: unknown;
}) | WorkspaceFileLocator & ({
  "type": "workspace-file";
  [key: string]: unknown;
}) | NativeLocator & ({
  "type": "native";
  [key: string]: unknown;
}) | UrlLocator & ({
  "type": "url";
  [key: string]: unknown;
}));

/** ArtifactPayload; `protocol.md` §5.5. */
export type ArtifactPayload = ({
  "action": ArtifactAction;
  "artifactId": Id;
  "locator": ArtifactLocator;
  "mediaType": Knowledge2;
  "producerToolCallId": (Id | (null));
  "revision": U64;
  "sizeBytes": Knowledge3;
  "title": Knowledge2;
  "type": ArtifactType;
  "verification": ArtifactVerification;
  [key: string]: unknown;
});

/** ArtifactType wire values; `protocol.md` §5.5. */
export type ArtifactType = ("file" | "image" | "html" | "url" | "native" | "diff");

/** ArtifactVerification wire values; `protocol.md` §5.5. */
export type ArtifactVerification = ("declared" | "read-verified" | "native-confirmed");

/** AttachMode wire values; `protocol.md` §3.1. */
export type AttachMode = ("observe" | "control");

/** AttachRef; `protocol.md` §3.1. */
export type AttachRef = ({
  "allowWake": BoolLiteral_false;
  "mode": AttachMode;
  "nativeRef": NativeRef;
  "processRef": ProcessRef;
  [key: string]: unknown;
});

/** BgInputDelivery wire values; `protocol.md` §4.1. */
export type BgInputDelivery = ("deferred-argv");

/** Output stream category; the byte values are fixed by `protocol.md` §7.4. */
export type BinaryChannel = ("tty-output" | "object-chunk" | "tty-input");

/** Decoded metadata for the 32-byte binary header; JSON is for fixtures only; §7.4. */
export type BinaryHeader = ({
  "channel": BinaryChannel;
  "offset": U64;
  "payloadLength": (number);
  "streamUuid": StreamUuid;
});

/** BlobLocator; `protocol.md` §5.5. */
export type BlobLocator = ({
  "digest": Digest;
  "objectId": Id;
  [key: string]: unknown;
});

export type BoolLiteral_false = false;

export type BoolLiteral_true = true;

/** Capability; `protocol.md` §3.2. */
export type Capability = ({
  "evidence": ((CapabilityEvidence)[]);
  "prerequisites": (((string))[]);
  "provision": CapabilityProvision;
  "reasonCode": (string);
  "scope": (((string))[]);
  "state": CapabilityState;
  [key: string]: unknown;
});

/** CapabilityEvidence; `protocol.md` §3.2. */
export type CapabilityEvidence = ({
  "digest": Knowledge;
  "ref": (string);
  "type": EvidenceType;
  [key: string]: unknown;
});

/** CapabilityName wire values; `protocol.md` §3.2. */
export type CapabilityName = ("resume" | "steer" | "queue" | "interrupt" | "model-switch" | "fork" | "structured-workflow" | "artifact" | "tty-attach" | "hooks" | "interactive-approval" | "question" | "plan-review" | "elicitation" | "live-attach" | "completion-native-turn" | "completion-task");

/** CapabilityProvision wire values; `protocol.md` §3.2. */
export type CapabilityProvision = ("native" | "emulated" | "unknown");

/** Complete capability record; `protocol.md` §3.2. Missing capabilities are invalid.  D-028 §6 added `queue` / `interrupt` alongside `steer`. They are `#[serde(default)]` to an `unknown` [`Capability`] so a snapshot written by a pre-D-028 peer still parses; "absent" is read as "not verified", never as unsupported. */
export type CapabilitySet = ({
  "artifact": Capability;
  "completion-native-turn": Capability;
  "completion-task": Capability;
  "elicitation": Capability;
  "fork": Capability;
  "hooks": Capability;
  "interactive-approval": Capability;
  "interrupt": Capability;
  "live-attach": Capability;
  "model-switch": Capability;
  "plan-review": Capability;
  "question": Capability;
  "queue": Capability;
  "resume": Capability;
  "steer": Capability;
  "structured-workflow": Capability;
  "tty-attach": Capability;
  [key: string]: unknown;
});

/** CapabilitySnapshot; `protocol.md` §3.2. */
export type CapabilitySnapshot = ({
  "adapterTransport": AdapterTransport;
  "adapterVersion": (string);
  "binaryDigest": Digest;
  "binaryVersion": (string);
  "capabilities": CapabilitySet;
  "driverKind": DriverKind;
  "id": Id;
  "nativeProtocolVersion": Knowledge2;
  "providerProfileRevision": U64;
  "settingsRevision": U64;
  [key: string]: unknown;
});

/** CapabilityState wire values; `protocol.md` §3.2. */
export type CapabilityState = ("supported" | "unsupported" | "unknown");

/** Native process carrier; changing it requires a new Instance; `protocol.md` §4.1. */
export type CarrierSpec = (({
  "type": "stdio";
}) | ({
  "backend": PtyBackend;
  "server": HerdrServer;
  "session": (string);
  "type": "pty";
}) | ({
  "argvInputPolicy": ArgvInputPolicy;
  "inputDelivery": BgInputDelivery;
  "type": "claude-bg";
}) | ({
  "type": "shell-pty";
}));

/** ChangeApplication wire values; `protocol.md` §5.2. */
export type ChangeApplication = ("proposed" | "applied" | "unknown");

/** Deferred first input for Claude background launch; `protocol.md` §4.1. */
export type ClaudeBgCarrier = ({
  "argvInputPolicy": ArgvInputPolicy;
  "inputDelivery": BgInputDelivery;
});

/** A Claude daemon job that may exist before its native session is known; §1.3. */
export type ClaudeBgRef = ({
  "jobId": (string);
});

/** ClaudeInteractionMode wire values; `protocol.md` §4.1. */
export type ClaudeInteractionMode = ("host" | "native-tty");

/** ClaudePermission; `protocol.md` §4.1. */
export type ClaudePermission = ({
  "interaction": ClaudeInteractionMode;
  "mode": ClaudePermissionMode;
  [key: string]: unknown;
});

/** ClaudePermissionMode wire values; `protocol.md` §4.1. */
export type ClaudePermissionMode = ("manual" | "auto" | "acceptEdits" | "dontAsk" | "plan" | "bypassPermissions");

/** ClaudeRef; `protocol.md` §1.3. */
export type ClaudeRef = ({
  "sessionId": (string);
});

/** CloseMode wire values; `protocol.md` §7.2. */
export type CloseMode = ("terminate");

/** Mutually exclusive Codex sandbox or permission profile; `protocol.md` §4.1. */
export type CodexExecution = (SandboxExecution | NamedPermissions);

/** CodexPermission; `protocol.md` §4.1. */
export type CodexPermission = ({
  "approvalPolicy": ApprovalPolicy;
  "approvalsReviewer": ApprovalsReviewer;
  "execution": CodexExecution;
  [key: string]: unknown;
});

/** CodexRef; `protocol.md` §1.3. */
export type CodexRef = ({
  "threadId": (string);
  [key: string]: unknown;
});

/** Command; `protocol.md` §2.5. */
export type Command = ({
  "acceptance": Knowledge7;
  "acceptedAt": Knowledge9;
  "actor": ActorRef;
  "authority": CommandAuthority;
  "commandId": CommandId;
  "createdAt": Timestamp;
  "dispatch": DispatchState;
  "expected": ExpectedState;
  "expiresAt": (Timestamp | (null));
  "forwardIntent": Knowledge6;
  "id": CommandId;
  "nodeReceipt": Knowledge10;
  "operation": CommandOperation;
  "origin": CommandOrigin;
  "payloadDigest": Digest;
  "payloadRef": Id;
  "queuedAt": Timestamp;
  "resolution": ResolutionState;
  "revision": U64;
  "settledAt": Knowledge9;
  "settlement": Knowledge8;
  "state": CommandState;
  "target": CommandTarget;
  "updatedAt": Timestamp;
  [key: string]: unknown;
});

/** CommandAuthority wire values; `protocol.md` §2.5. */
export type CommandAuthority = ("hub-inbox" | "node-ledger");

/** CommandEnvelope; `protocol.md` §7.2. */
export type CommandEnvelope = ({
  "commandId": CommandId;
  "expected"?: (ExpectedState | (null));
  "expiresAt"?: (Timestamp | (null));
  "payload": unknown;
  [key: string]: unknown;
});

/** CommandEnvelope; `protocol.md` §7.2. */
export type CommandEnvelope10 = ({
  "commandId": CommandId;
  "expected"?: (ExpectedState | (null));
  "expiresAt"?: (Timestamp | (null));
  "payload": InstanceConfigureParams;
  [key: string]: unknown;
});

/** CommandEnvelope; `protocol.md` §7.2. */
export type CommandEnvelope11 = ({
  "commandId": CommandId;
  "expected"?: (ExpectedState | (null));
  "expiresAt"?: (Timestamp | (null));
  "payload": InstanceForkParams;
  [key: string]: unknown;
});

/** CommandEnvelope; `protocol.md` §7.2. */
export type CommandEnvelope12 = ({
  "commandId": CommandId;
  "expected"?: (ExpectedState | (null));
  "expiresAt"?: (Timestamp | (null));
  "payload": InstanceCancelParams;
  [key: string]: unknown;
});

/** CommandEnvelope; `protocol.md` §7.2. */
export type CommandEnvelope13 = ({
  "commandId": CommandId;
  "expected"?: (ExpectedState | (null));
  "expiresAt"?: (Timestamp | (null));
  "payload": InstanceCloseParams;
  [key: string]: unknown;
});

/** CommandEnvelope; `protocol.md` §7.2. */
export type CommandEnvelope14 = ({
  "commandId": CommandId;
  "expected"?: (ExpectedState | (null));
  "expiresAt"?: (Timestamp | (null));
  "payload": InteractionRespondParams;
  [key: string]: unknown;
});

/** CommandEnvelope; `protocol.md` §7.2. */
export type CommandEnvelope15 = ({
  "commandId": CommandId;
  "expected"?: (ExpectedState | (null));
  "expiresAt"?: (Timestamp | (null));
  "payload": TtyWriteParams;
  [key: string]: unknown;
});

/** CommandEnvelope; `protocol.md` §7.2. */
export type CommandEnvelope2 = ({
  "commandId": CommandId;
  "expected"?: (ExpectedState | (null));
  "expiresAt"?: (Timestamp | (null));
  "payload": WorkspaceRegisterParams;
  [key: string]: unknown;
});

/** CommandEnvelope; `protocol.md` §7.2. */
export type CommandEnvelope3 = ({
  "commandId": CommandId;
  "expected"?: (ExpectedState | (null));
  "expiresAt"?: (Timestamp | (null));
  "payload": WorktreeCreateParams;
  [key: string]: unknown;
});

/** CommandEnvelope; `protocol.md` §7.2. */
export type CommandEnvelope4 = ({
  "commandId": CommandId;
  "expected"?: (ExpectedState | (null));
  "expiresAt"?: (Timestamp | (null));
  "payload": WorktreeRemoveParams;
  [key: string]: unknown;
});

/** CommandEnvelope; `protocol.md` §7.2. */
export type CommandEnvelope5 = ({
  "commandId": CommandId;
  "expected"?: (ExpectedState | (null));
  "expiresAt"?: (Timestamp | (null));
  "payload": InstanceCreateParams;
  [key: string]: unknown;
});

/** CommandEnvelope; `protocol.md` §7.2. */
export type CommandEnvelope6 = ({
  "commandId": CommandId;
  "expected"?: (ExpectedState | (null));
  "expiresAt"?: (Timestamp | (null));
  "payload": InstanceAttachParams;
  [key: string]: unknown;
});

/** CommandEnvelope; `protocol.md` §7.2. */
export type CommandEnvelope7 = ({
  "commandId": CommandId;
  "expected"?: (ExpectedState | (null));
  "expiresAt"?: (Timestamp | (null));
  "payload": InstanceOpenTerminalParams;
  [key: string]: unknown;
});

/** CommandEnvelope; `protocol.md` §7.2. */
export type CommandEnvelope8 = ({
  "commandId": CommandId;
  "expected"?: (ExpectedState | (null));
  "expiresAt"?: (Timestamp | (null));
  "payload": InstanceResumeParams;
  [key: string]: unknown;
});

/** CommandEnvelope; `protocol.md` §7.2. */
export type CommandEnvelope9 = ({
  "commandId": CommandId;
  "expected"?: (ExpectedState | (null));
  "expiresAt"?: (Timestamp | (null));
  "payload": InstanceSendParams;
  [key: string]: unknown;
});

export type CommandId = (string);

/** CommandListParams; `protocol.md` §7.2. */
export type CommandListParams = ({
  "cursor"?: (string | null);
  "instanceId"?: (InstanceId | (null));
  "limit": (number);
  "resolution"?: (ResolutionState | (null));
  [key: string]: unknown;
});

/** CommandOperation wire values; `protocol.md` §2.5. */
export type CommandOperation = ("instance.create" | "instance.attach" | "instance.open_terminal" | "instance.resume" | "instance.send" | "instance.cancel" | "instance.close" | "instance.fork" | "instance.configure" | "interaction.respond" | "tty.write" | "workspace.register" | "worktree.create" | "worktree.remove");

/** CommandOrigin wire values; `protocol.md` §2.5. */
export type CommandOrigin = ("ui" | "bot" | "mcp" | "cli" | "system");

/** CommandParams; `protocol.md` §7.2. */
export type CommandParams = ({
  "commandId": CommandId;
  [key: string]: unknown;
});

/** CommandResult; `protocol.md` §7.2. */
export type CommandResult = ({
  "command": Command;
  "relatedCommandIds": ((CommandId)[]);
  [key: string]: unknown;
});

/** CommandState wire values; `protocol.md` §2.5. */
export type CommandState = ("queued" | "accepted" | "settled");

/** CommandTarget; `protocol.md` §2.5. */
export type CommandTarget = ({
  "hostId": HostId;
  "instanceId": (InstanceId | (null));
  "runId": (RunId | (null));
  [key: string]: unknown;
});

/** CommittedAnswer; `protocol.md` §2.6. */
export type CommittedAnswer = ({
  "actor": ActorRef;
  "commandId": CommandId;
  "committedAt": Timestamp;
  "value": InteractionAnswer;
  [key: string]: unknown;
});

/** Completeness wire values; `protocol.md` §5.1. */
export type Completeness = ("structured" | "partial" | "screen-derived" | "opaque");

/** CompletionScope wire values; `protocol.md` §2.4. */
export type CompletionScope = ("native-turn" | "task");

/** ConnectionLease; `protocol.md` §7.1. */
export type ConnectionLease = ({
  "expiresAt": Timestamp;
  "fence": U64;
  "leaseId": Id;
  [key: string]: unknown;
});

/** Connectivity wire values; `protocol.md` §2.3. */
export type Connectivity = ("connected" | "disconnected" | "reconciling");

/** ContentBlock; `protocol.md` §5.2. */
export type ContentBlock = (TextBlock & ({
  "type": "text";
  [key: string]: unknown;
}) | MediaBlock & ({
  "type": "image";
  [key: string]: unknown;
}) | MediaBlock & ({
  "type": "audio";
  [key: string]: unknown;
}) | MediaBlock & ({
  "type": "file";
  [key: string]: unknown;
}) | ResourceBlock & ({
  "type": "resource";
  [key: string]: unknown;
}) | OpaqueBlock & ({
  "type": "opaque";
  [key: string]: unknown;
}));

/** ContentStatus wire values; `protocol.md` §5.2. */
export type ContentStatus = ("queued" | "streaming" | "complete" | "interrupted" | "unknown");

/** Context-size requirements of a task; §4.3. */
export type ContextNeed = ({
  "expectedInputTokens"?: (U64 | (null));
  "needsLongContext"?: (boolean);
  "repoScope"?: (string | null);
  [key: string]: unknown;
});

/** ConversationNode; `protocol.md` §7.3. */
export type ConversationNode = (({
  "kind": "message";
  "payload": MessagePayload;
  [key: string]: unknown;
}) | ({
  "kind": "thought";
  "payload": ThoughtPayload;
  [key: string]: unknown;
}) | ({
  "kind": "tool_call";
  "payload": ToolCallPayload;
  [key: string]: unknown;
}) | ({
  "kind": "tool_result";
  "payload": ToolResultPayload;
  [key: string]: unknown;
}));

/** Cost; `protocol.md` §5.5. */
export type Cost = ({
  "amount": (string);
  "currency": (string);
  [key: string]: unknown;
});

/** CreateInitialInput; `protocol.md` §7.2. */
export type CreateInitialInput = (PromptInput & ({
  "type": "prompt";
  [key: string]: unknown;
}));

/** CreateWorktree; `protocol.md` §4.1. */
export type CreateWorktree = ({
  "baseOid": (string);
  "branch": (string);
  "worktreeId": WorktreeId;
  [key: string]: unknown;
});

/** CredentialEnv; `protocol.md` §4.1. */
export type CredentialEnv = ({
  "credentialRef": Id;
  "version": U64;
  [key: string]: unknown;
});

/** DeadlineSource wire values; `protocol.md` §2.6. */
export type DeadlineSource = ("native" | "runtime-policy" | "none" | "unknown");

/** DecisionEffect wire values; `protocol.md` §5.4. */
export type DecisionEffect = ("allow-once" | "allow-session" | "deny" | "cancel" | "native-specific");

/** DecisionOption; `protocol.md` §5.4. */
export type DecisionOption = ({
  "effect": DecisionEffect;
  "id": (string);
  "label": (string);
  "nativeValueRef": Id;
  [key: string]: unknown;
});

/** DeliveryState wire values; `protocol.md` §2.6. */
export type DeliveryState = ("not-sent" | "intent-durable" | "written" | "confirmed" | "rejected" | "unknown");

/** Result of reconciling a diff against a task's `owns[]`; design §4.3 (`scopeCheck.diffMustStayWithin: owns`) — the single highest-value new primitive (playbook), and the pure function the later merge gate calls. */
export type DiffScopeCheck = ({
  "checked": (number);
  "violations"?: (((string))[]);
  "within": (boolean);
  [key: string]: unknown;
});

export type Digest = (string);

/** DispatchState wire values; `protocol.md` §2.5. */
export type DispatchState = ("not-dispatched" | "intent-durable" | "transport-written" | "native-acknowledged");

/** DriverCapabilitiesParams; `protocol.md` §7.2. */
export type DriverCapabilitiesParams = ({
  "binaryRef": Id;
  "driverKind": DriverKind;
  "profileRef": ProfileRef;
  [key: string]: unknown;
});

/** DriverDescriptor; `protocol.md` §3.2. */
export type DriverDescriptor = ({
  "adapterVersion": (string);
  "binaryDigest": Digest;
  "binaryPath": (string);
  "binaryVersion": (string);
  "capabilities": CapabilitySnapshot;
  "kind": DriverKind;
  "launchable": (boolean);
  "reasonCode": (string);
  [key: string]: unknown;
});

/** DriverInput; `protocol.md` §3.1. */
export type DriverInput = (PromptInput & ({
  "type": "prompt";
  [key: string]: unknown;
}) | SteerInput & ({
  "type": "steer";
  [key: string]: unknown;
}) | ModelSwitchInput & ({
  "type": "model-switch";
  [key: string]: unknown;
}));

/** DriverKind wire values; `protocol.md` §3.1. */
export type DriverKind = ("claude-print" | "claude-pty" | "claude-bg" | "codex-appserver" | "grok-acp" | "agy-print" | "generic-pty" | "shell-pty");

/** The effective-model half of a [`ModelPayload`]: the resolved id plus what established it. */
export type EffectiveModel = ({
  "id": (string);
  "observedAt": Timestamp;
  "source": EffortSource;
  [key: string]: unknown;
});

/** Effective effort, read back from a Claude assistant transcript record (`effort` / `perTurnEffort`); D-028 §9.1.  This is the *observed* tier, never the requested one. Claude reports `ultracode` sessions as level `xhigh` and does not repeat the workflow flag on assistant records, so `ultracode` is `None` unless the observation path has positive evidence (e.g. an immediately preceding `/effort ultracode` switch the driver itself made). */
export type EffortEffective = ({
  "name": EffortName;
  "observedAt": Timestamp;
  "source": EffortSource;
  "ultracode"?: (boolean | null);
  [key: string]: unknown;
});

/** EffortName wire values; `protocol.md` §4.1. */
export type EffortName = ("minimal" | "low" | "medium" | "high" | "xhigh" | "max" | "ultra");

/** EffortPayload; `protocol.md` §5.1 (D-028 §9.1).  Emitted whenever an assistant transcript record's `effort` / `perTurnEffort` is observed. Unchanged values are deduped by the emitting driver, so the Hub only sees edges. The UI renders from this observation — never from the requested selection. */
export type EffortPayload = ({
  "effective": EffortEffective;
  "raw"?: (string | null);
  "requested"?: (EffortSelection | (null));
  [key: string]: unknown;
});

/** Native effort selection; `protocol.md` §4.1 (D-028 §9.1).  Six Codex levels (`low..=ultra`), five Claude levels (`low..=max`), and an orthogonal Claude `ultracode` boolean. `ultracode` is `xhigh` plus dynamic workflow, is session-only, and is never persisted as a level name. It is independent of Codex's `ultra` level.  [`EffortName`] additionally carries the legacy `minimal` input, which maps to `low` for Codex and is rejected at Claude, agy, and Grok launches.  Deserialization accepts the pre-D-028 shape `{index, name}` and normalizes legacy tier **names** through [`normalize_legacy_effort`], so a stored row or an old client keeps working. Legacy normalization is **per harness**:  | legacy `name` | harness | normalized | | --- | --- | --- | | `default` | any | `low` | | `think` | claude | `high` | | `think-hard` | claude | `xhigh` | | `minimal` | codex | `low` | | `ultra` | codex | `ultra` (a real Codex level; rejected by other harnesses) | | `quick` / `standard` / `max` | grok | `low` / `medium` / `xhigh` | | `ultracode` | claude | `xhigh` + `ultracode: true` | | anything unrecognized | any | the harness default (`high` for claude, `medium` otherwise) |  Normalization is by **name**, never by index: the legacy tables had different lengths per harness, so index 3 meant `ultracode` for Claude and `ultra` for Codex. `index` on the wire is therefore ignored on read and not written back. */
export type EffortSelection = ({
  "name": EffortName;
  "ultracode": (boolean);
  [key: string]: unknown;
});

/** EffortSource wire values; `protocol.md` §9.1. */
export type EffortSource = ("launch" | "slash" | "remuda" | "unknown");

/** ElicitationAction wire values; `protocol.md` §5.4. */
export type ElicitationAction = ("accept" | "decline" | "cancel");

/** ElicitationAnswer; `protocol.md` §5.4. */
export type ElicitationAnswer = ({
  "action": ElicitationAction;
  "content": unknown;
  [key: string]: unknown;
});

/** ElicitationMode wire values; `protocol.md` §5.4. */
export type ElicitationMode = ("form" | "url" | "native-extension");

/** ElicitationRequest; `protocol.md` §5.4. */
export type ElicitationRequest = ({
  "allowedActions": ((ElicitationAction)[]);
  "mode": ElicitationMode;
  "nativeExtension": (string | null);
  "schemaDialect": (string | null);
  "schemaRef": (Id | (null));
  "title": (string);
  "url": (string | null);
  [key: string]: unknown;
});

/** EmptyResult; `protocol.md` §7.2. */
export type EmptyResult = ({
  [key: string]: unknown;
});

/** EntityLifecycle; `protocol.md` §5.5. */
export type EntityLifecycle = (({
  "entity": Host;
  "entityType": "host";
  [key: string]: unknown;
}) | ({
  "entity": Workspace;
  "entityType": "workspace";
  [key: string]: unknown;
}) | ({
  "entity": Instance;
  "entityType": "instance";
  [key: string]: unknown;
}) | ({
  "entity": Run;
  "entityType": "run";
  [key: string]: unknown;
}) | ({
  "entity": Command;
  "entityType": "command";
  [key: string]: unknown;
}) | ({
  "entity": Interaction;
  "entityType": "interaction";
  [key: string]: unknown;
})) & ({
  "entityId": Id;
  "evidenceEventIds": ((EventId)[]);
  "previousState": (string | null);
  "reasonCode": (string);
  "revision": U64;
  "state": (string);
  [key: string]: unknown;
});

/** EntityMeta; `protocol.md` §1.1. */
export type EntityMeta = ({
  "createdAt": Timestamp;
  "id": Id;
  "revision": U64;
  "updatedAt": Timestamp;
  [key: string]: unknown;
});

/** EnvBinding; `protocol.md` §4.1. */
export type EnvBinding = (LiteralEnv & ({
  "source": "literal";
  [key: string]: unknown;
}) | CredentialEnv & ({
  "source": "credential";
  [key: string]: unknown;
}) | HostEnv & ({
  "source": "host-env";
  [key: string]: unknown;
}));

/** EnvVisibility wire values; `protocol.md` §4.1. */
export type EnvVisibility = ("private");

/** ErrorCode wire values; `protocol.md` §9.1. */
export type ErrorCode = ("UNAUTHENTICATED" | "SCOPE_DENIED" | "HOST_OFFLINE" | "OWNER_FENCED" | "PROTOCOL_VERSION_UNSUPPORTED" | "SCHEMA_VERSION_UNSUPPORTED" | "CAPABILITY_UNSUPPORTED" | "CAPABILITY_UNKNOWN" | "NATIVE_FEATURE_DISABLED" | "BINARY_CHANGED" | "INVALID_LAUNCH_SPEC" | "SETTINGS_ISOLATION_UNAVAILABLE" | "PROVIDER_PROTOCOL_MISMATCH" | "PROVIDER_UNAVAILABLE" | "CREDENTIAL_UNAVAILABLE" | "WORKSPACE_NOT_FOUND" | "WORKSPACE_BUSY" | "WORKTREE_BUSY" | "WORKTREE_DIRTY" | "NATIVE_SESSION_NOT_FOUND" | "NATIVE_SESSION_OWNED" | "NATIVE_GENERATION_MISMATCH" | "ATTACH_WOULD_WAKE" | "CONTROL_UNAVAILABLE" | "COMMAND_ID_CONFLICT" | "COMMAND_EXPIRED" | "COMMAND_OUTCOME_UNKNOWN" | "RUN_NOT_ACTIVE" | "INTERACTION_ALREADY_ANSWERED" | "INTERACTION_STALE" | "INTERACTION_EXPIRED" | "INTERACTION_SCHEMA_UNSUPPORTED" | "INTERACTION_NOT_ANSWERABLE" | "INVALID_ANSWER" | "NATIVE_RESPONSE_UNKNOWN" | "CURSOR_EXPIRED" | "JOURNAL_GAP" | "JOURNAL_DIVERGED" | "JOURNAL_UNAVAILABLE" | "STATE_UNKNOWN" | "RESOURCE_LIMIT" | "TTY_LEASE_LOST" | "TTY_HISTORY_GAP" | "OBJECT_NOT_FOUND" | "OBJECT_REVISION_MISMATCH" | "NATIVE_PROTOCOL_ERROR" | "WAIT_TIMEOUT");

/** ErrorDetails; `protocol.md` §9.1. */
export type ErrorDetails = ({
  "actualGeneration"?: (U64 | (null));
  "commandId"?: (CommandId | (null));
  "evidenceEventIds"?: ((EventId)[] | null);
  "expectedGeneration"?: (U64 | (null));
  "instanceId"?: (InstanceId | (null));
  "interactionId"?: (InteractionId | (null));
  "nativeErrorRef"?: (Id | (null));
  "retryAfterMs"?: (number | null);
  [key: string]: unknown;
});

export type EventId = (string);

/** EventsAckParams; `protocol.md` §7.3. */
export type EventsAckParams = ({
  "journalId": Id;
  "subscriptionId": Id;
  "throughSeq": U64;
  [key: string]: unknown;
});

/** EventsAckResult; `protocol.md` §7.3. */
export type EventsAckResult = ({
  "acknowledgedSeq": U64;
  [key: string]: unknown;
});

/** EventsBatch; `protocol.md` §7.3. */
export type EventsBatch = ({
  "durableSeq": U64;
  "events": NonEmpty_JournalEvent;
  "fromSeq": U64;
  "journalId": Id;
  "subscriptionId": Id;
  "toSeq": U64;
  [key: string]: unknown;
});

/** EventsReadParams; `protocol.md` §7.3. */
export type EventsReadParams = ({
  "afterSeq"?: (U64 | (null));
  "beforeSeq"?: (U64 | (null));
  "cursor"?: (string | null);
  "journalId": Id;
  "limit": (number);
  [key: string]: unknown;
});

/** EventsReadResult; `protocol.md` §7.3. */
export type EventsReadResult = ({
  "durableSeq": U64;
  "events": ((JournalEvent)[]);
  "floorSeq": U64;
  "nextCursor": (string | null);
  [key: string]: unknown;
});

/** EventsSubscribeParams; `protocol.md` §7.3. */
export type EventsSubscribeParams = ({
  "afterSeq": (U64 | (null));
  "batchLimit": (number);
  "journalId": Id;
  "projectionVersion": (string);
  "snapshot": SnapshotMode;
  [key: string]: unknown;
});

/** EventsSubscribeResult; `protocol.md` §7.3. */
export type EventsSubscribeResult = ({
  "connectionId": Id;
  "durableSeq": U64;
  "floorSeq": U64;
  "journalId": Id;
  "nextCursor": (string | null);
  "replayFromSeq": U64;
  "snapshot": (Snapshot | (null));
  "subscriptionId": Id;
  [key: string]: unknown;
});

/** EvidenceType wire values; `protocol.md` §3.2. */
export type EvidenceType = ("fixture" | "native-negotiation" | "source" | "help");

/** ExecutionState wire values; `protocol.md` §9.1. */
export type ExecutionState = ("not-dispatched" | "possibly-dispatched" | "accepted" | "settled" | "not-applicable");

/** ExecutorRef; `protocol.md` §5.2. */
export type ExecutorRef = ({
  "hostId": HostId;
  "nativeAgentId": (string | null);
  "workspaceId": (WorkspaceId | (null));
  [key: string]: unknown;
});

/** ExistingWorktree; `protocol.md` §4.1. */
export type ExistingWorktree = ({
  "worktreeId": WorktreeId;
  [key: string]: unknown;
});

/** ExpectedState; `protocol.md` §2.5. */
export type ExpectedState = ({
  "instanceRevision"?: (U64 | (null));
  "interactionVersion"?: (U64 | (null));
  "ownerFence"?: (U64 | (null));
  "processGeneration"?: (U64 | (null));
  "runGeneration"?: (U64 | (null));
  [key: string]: unknown;
});

/** FileChange; `protocol.md` §5.2. */
export type FileChange = ({
  "application": ChangeApplication;
  "diff": (string);
  "path": (string);
  [key: string]: unknown;
});

/** FileCursor; `protocol.md` §5.1. */
export type FileCursor = ({
  "digest": Digest;
  "fileGeneration": U64;
  "fileIdentity": Id;
  "length": U64;
  "offset": U64;
  [key: string]: unknown;
});

/** ForkBoundary; `protocol.md` §7.2. */
export type ForkBoundary = ({
  "type": ForkBoundaryType;
  [key: string]: unknown;
});

/** ForkBoundaryType wire values; `protocol.md` §7.2. */
export type ForkBoundaryType = ("latest-terminal");

/** ForwardIntent; `protocol.md` §2.5. */
export type ForwardIntent = ({
  "createdAt": Timestamp;
  "hostId": HostId;
  "hubRevision": U64;
  "nodeEpoch": Id;
  [key: string]: unknown;
});

/** `gate.cancel` params. */
export type GateCancelParams = ({
  "jobId": (string);
  [key: string]: unknown;
});

/** One streamed Node→Hub job event. */
export type GateEventKind = (({
  "kind": "phase";
  "phase": (string);
  [key: string]: unknown;
}) | ({
  "kind": "step";
  "step": GateStep;
  [key: string]: unknown;
}) | ({
  "kind": "log";
  "message": (string);
  [key: string]: unknown;
}) | ({
  "kind": "finished";
  "result": GateRunResult;
  [key: string]: unknown;
}));

/** Params of the Node-originated `gate.event` notification. */
export type GateEventParams = (({
  "kind": "phase";
  "phase": (string);
  [key: string]: unknown;
}) | ({
  "kind": "step";
  "step": GateStep;
  [key: string]: unknown;
}) | ({
  "kind": "log";
  "message": (string);
  [key: string]: unknown;
}) | ({
  "kind": "finished";
  "result": GateRunResult;
  [key: string]: unknown;
})) & ({
  "jobId": (string);
  [key: string]: unknown;
});

/** One queued gate/land job (Hub document; coordinator-hierarchy.md §8 row 6). */
export type GateJob = ({
  "attempts": (number);
  "baseSha"?: (string | null);
  "branch": (string);
  "currentMainSha"?: (string | null);
  "error"?: (string | null);
  "failedStep"?: (string | null);
  "finishedAt"?: (Timestamp | (null));
  "headSha"?: (string | null);
  "hostId"?: (HostId | (null));
  "id": GateJobId;
  "keepLogs": (boolean);
  "laneId"?: (string | null);
  "logObjectId"?: (string | null);
  "mergeSha"?: (string | null);
  "mode": GateMode;
  "projectId": ProjectId;
  "queuedAt": Timestamp;
  "reason"?: (string | null);
  "requestedBy": (string);
  "startedAt"?: (Timestamp | (null));
  "state": GateJobState;
  "steps": ((GateStep)[]);
  "thenCommand"?: (string | null);
  "thenOutput"?: (string | null);
  "web": GateWebMode;
  [key: string]: unknown;
});

export type GateJobId = (string);

/** Gate job lifecycle: queued → running → passed|failed|landed, with cancel. */
export type GateJobState = ("queued" | "running" | "passed" | "failed" | "landed" | "canceling" | "canceled");

/** What the job does: verify only, or verify-then-land. */
export type GateMode = ("verify" | "land");

/** Bounded failure evidence for one gate run, stored by the Hub as an `obj_…` log object (never inlined into the job row). For a failed run the Node populates it for the failed step; with `--keep-logs` a green run gets a `kept` log of the whole-run tail. */
export type GateRunLog = ({
  "attempts": (number);
  "capturedLines": (number);
  "headline": (string);
  "kind": (string);
  "step": (string);
  "summary": (((string))[]);
  "tail": (((string))[]);
  "truncated": (boolean);
  [key: string]: unknown;
});

/** `gate.run` params: everything the lane runner needs, derived from the project's `ProjectGateLane` (set once, never re-typed per run). */
export type GateRunParams = ({
  "baseBranch": (string);
  "binary"?: (string | null);
  "branch": (string);
  "env": ({
  [key: string]: (string);
});
  "gateTimeoutSecs": (number);
  "jobId": (string);
  "keepLogs": (boolean);
  "laneId": (string);
  "lockPath"?: (string | null);
  "mode": (string);
  "ports"?: (string | null);
  "push": (boolean);
  "pwEndpoint"?: (string | null);
  "repoPath": (string);
  "targetDir": (string);
  "timeouts": ({
  [key: string]: (number);
});
  "toolchainPath"?: (string | null);
  "web": (string);
  [key: string]: unknown;
});

/** `gate.run` verdict, mirrored by the terminal `finished` event. */
export type GateRunResult = ({
  "baseSha"?: (string | null);
  "currentMainSha"?: (string | null);
  "error"?: (string | null);
  "failedStep"?: (string | null);
  "headSha"?: (string | null);
  "jobId": (string);
  "mergeSha"?: (string | null);
  "reason"?: (string | null);
  "runLog"?: (GateRunLog | (null));
  "status": (string);
  "steps": ((GateStep)[]);
  [key: string]: unknown;
});

/** One gate step result; same JSON shape as `remuda merge --gate --json`. */
export type GateStep = ({
  "attempts": (number);
  "durationMs": (number);
  "error"?: (string | null);
  "name": (string);
  "reason"?: (string | null);
  "retried": (boolean);
  "status": (string);
  [key: string]: unknown;
});

/** `gate.then` params: post-land command on the project's home host. */
export type GateThenParams = ({
  "command": (string);
  "cwd"?: (string | null);
  "env": ({
  [key: string]: (string);
});
  "jobId": (string);
  "timeoutSecs": (number);
  [key: string]: unknown;
});

/** `gate.then` result. */
export type GateThenResult = ({
  "exitCode": (number);
  "jobId": (string);
  "output": (string);
  [key: string]: unknown;
});

/** Web gate selection. */
export type GateWebMode = ("auto" | "always" | "never");

/** GenericPermission; `protocol.md` §4.1. */
export type GenericPermission = ({
  "mode": GenericPermissionMode;
  [key: string]: unknown;
});

/** GenericPermissionMode wire values; `protocol.md` §4.1. */
export type GenericPermissionMode = ("native");

/** GrantVerb wire values; `protocol.md` §2.5. */
export type GrantVerb = ("dispatch" | "land" | "spend" | "address-owner");

/** GrokPermission; `protocol.md` §4.1. */
export type GrokPermission = ({
  "mode": GrokPermissionMode;
  [key: string]: unknown;
});

/** GrokPermissionMode wire values; `protocol.md` §4.1. */
export type GrokPermissionMode = ("native-prompt" | "auto" | "always-approve");

/** HeartbeatParams; `protocol.md` §7.2. */
export type HeartbeatParams = ({
  "connectionId": Id;
  "instanceWatermarks": ((JournalWatermark)[]);
  "leaseId": Id;
  "registryWatermarks": ((JournalWatermark)[]);
  [key: string]: unknown;
});

/** HeartbeatResult; `protocol.md` §7.2. */
export type HeartbeatResult = ({
  "leaseExpiresAt": Timestamp;
  "serverTime": Timestamp;
  [key: string]: unknown;
});

/** HelloParams; `protocol.md` §7.1. */
export type HelloParams = ({
  "features": (((string))[]);
  "hostId": HostId;
  "nodeEpoch": Id;
  "nodeVersion": (string);
  "observationSchemaMajors": (((number))[]);
  "protocol": ProtocolRange;
  "resumeCursors": ((ResumeCursor)[]);
  [key: string]: unknown;
});

/** HelloResult; `protocol.md` §7.1. */
export type HelloResult = ({
  "connectionId": Id;
  "features": (((string))[]);
  "lease": ConnectionLease;
  "limits": TransportLimits;
  "observationSchemaMajor": SchemaVersion;
  "protocol": ProtocolVersion;
  "reconcileRequired": (boolean);
  "serverEpoch": Id;
  [key: string]: unknown;
});

/** Herdr pane identity within a named server session; `protocol.md` §1.3. */
export type HerdrRef = ({
  "binaryPath": (string);
  "digest": Digest;
  "paneId": (string);
  "protocolVersion": (string);
  "representation": HerdrRepresentation;
  "serverEpoch": Id;
  "serverIdentity": Id;
  "session": (string);
  "version": (string);
  [key: string]: unknown;
});

/** HerdrRepresentation wire values; `protocol.md` §1.3. */
export type HerdrRepresentation = ("rendered-ansi");

/** Pinned Herdr binary and current server lifetime; `protocol.md` §1.3. */
export type HerdrServer = ({
  "binaryPath": (string);
  "digest": Digest;
  "protocolVersion": (string);
  "representation": HerdrRepresentation;
  "serverEpoch": Id;
  "serverIdentity": Id;
  "version": (string);
  [key: string]: unknown;
});

/** HistoryCoverage; `protocol.md` §7.3. */
export type HistoryCoverage = ({
  "complete": (boolean);
  "earliestRetainedSeq": U64;
  [key: string]: unknown;
});

/** HookCursor; `protocol.md` §5.1. */
export type HookCursor = ({
  "frame": U64;
  "invocationId": Id;
  [key: string]: unknown;
});

/** Host; `protocol.md` §2.1. */
export type Host = ({
  "createdAt": Timestamp;
  "driverInventory": ((DriverDescriptor)[]);
  "durableSeq": U64;
  "id": HostId;
  "identityKeyId": Id;
  "journalId": Id;
  "label": (string);
  "lastSeenAt": Knowledge9;
  "leaseExpiresAt": Knowledge9;
  "nodeEpoch": Knowledge15;
  "nodeVersion": Knowledge2;
  "ownerPrincipalId": Id;
  "platform": Knowledge14;
  "revision": U64;
  "state": HostState;
  "transport": HostTransport;
  "updatedAt": Timestamp;
  [key: string]: unknown;
});

/** HostEnv; `protocol.md` §4.1. */
export type HostEnv = ({
  "name": (string);
  [key: string]: unknown;
});

/** One directory entry returned by `host.files.list`. */
export type HostFileEntry = ({
  "kind": HostFileKind;
  "mode": (number);
  "mtime": (number);
  "name": (string);
  "size": (number);
  [key: string]: unknown;
});

/** Kind of one [`HostFileEntry`], from `lstat` (symlinks are not followed). */
export type HostFileKind = ("file" | "dir" | "symlink" | "other");

/** `host.files.read` result. Bytes stay behind `GET /v1/objects/{objectId}`. */
export type HostFileReadResult = ({
  "digest": (string);
  "objectId": (string);
  "size": (number);
  [key: string]: unknown;
});

/** `host.files.list` result: the canonical directory and its entries. */
export type HostFilesListResult = ({
  "entries": ((HostFileEntry)[]);
  "path": (string);
  "workspaceId": (string);
  [key: string]: unknown;
});

/** Shared selector for the read-only host-file RPCs: a registered workspace plus a workspace-relative path. */
export type HostFilesParams = ({
  "relPath"?: (string | null);
  "workspaceId": (string);
  [key: string]: unknown;
});

export type HostId = (string);

/** HostParams; `protocol.md` §7.2. */
export type HostParams = ({
  "hostId": HostId;
  [key: string]: unknown;
});

/** HostReportParams; `protocol.md` §7.2. */
export type HostReportParams = ({
  "driverInventory": ((DriverDescriptor)[]);
  "hostId": HostId;
  "instanceIds": ((InstanceId)[]);
  "nodeEpoch": Id;
  "platform": Platform;
  "workspaceIds": ((WorkspaceId)[]);
  [key: string]: unknown;
});

/** HostReportResult; `protocol.md` §7.2. */
export type HostReportResult = ({
  "hostRevision": U64;
  "registrySeq": U64;
  [key: string]: unknown;
});

/** HostState wire values; `protocol.md` §2.1. */
export type HostState = ("enrolled" | "online" | "offline" | "reconciling" | "retired");

/** HostTransport; `protocol.md` §2.1. */
export type HostTransport = ({
  "endpointRef": Id;
  "mode": HostTransportMode;
  [key: string]: unknown;
});

/** HostTransportMode wire values; `protocol.md` §2.1. */
export type HostTransportMode = ("outbound-wss" | "ssh-tunnel");

export type Id = (string);

/** InputAccounting wire values; `protocol.md` §5.5. */
export type InputAccounting = ("total-including-cache" | "uncached" | "provider-specific" | "unknown");

/** InputDelivery wire values; `protocol.md` §4.1. */
export type InputDelivery = ("stdio" | "tty" | "deferred-argv");

/** InputOrigin wire values; `protocol.md` §3.1. */
export type InputOrigin = ("human" | "bot" | "agent");

/** Instance; `protocol.md` §2.3. */
export type Instance = ({
  "activeRunIds": ((RunId)[]);
  "activity": Knowledge18;
  "activityEvidenceEventIds": ((EventId)[]);
  "capabilities": CapabilitySnapshot;
  "connectivity": Connectivity;
  "createdAt": Timestamp;
  "driver": DriverKind;
  "durableSeq": U64;
  "exit": Knowledge19;
  "hostId": HostId;
  "id": InstanceId;
  "journalId": Id;
  "kind": AgentKind;
  "lastError"?: (string | null);
  "launchId": Knowledge15;
  "launchedBy"?: (LaunchedBy | (null));
  "lifecycle": InstanceLifecycle;
  "mode"?: (InstanceMode | (null));
  "nativeRef": NativeRef;
  "ownerFence": U64;
  "ownership": Ownership;
  "parent": (InstanceParent | (null));
  "processRef": ProcessRef;
  "promotedAt"?: (Timestamp | (null));
  "revision": U64;
  "specRevision": U64;
  "updatedAt": Timestamp;
  "workspaceId": WorkspaceId;
  [key: string]: unknown;
});

/** InstanceAttachParams; `protocol.md` §7.2. */
export type InstanceAttachParams = ({
  "instanceId": InstanceId;
  "ref": AttachRef;
  [key: string]: unknown;
});

/** InstanceCancelParams; `protocol.md` §7.2. */
export type InstanceCancelParams = ({
  "instanceId": InstanceId;
  "runId": RunId;
  [key: string]: unknown;
});

/** InstanceCloseParams; `protocol.md` §7.2. */
export type InstanceCloseParams = ({
  "instanceId": InstanceId;
  "mode": CloseMode;
  "retainNativeSession": BoolLiteral_true;
  [key: string]: unknown;
});

/** InstanceConfigureParams; `protocol.md` §7.2. */
export type InstanceConfigureParams = ({
  "effective": ModelEffective;
  "effort"?: (EffortSelection | (null));
  "instanceId": InstanceId;
  "modelId": (string);
  "permissionMode"?: (string | null);
  [key: string]: unknown;
});

/** InstanceCreateParams; `protocol.md` §7.2. */
export type InstanceCreateParams = ({
  "initialInput"?: (CreateInitialInput | (null));
  "instanceId": InstanceId;
  "spec": InstanceSpec;
  [key: string]: unknown;
});

/** InstanceCreateResult; `protocol.md` §7.2. */
export type InstanceCreateResult = ({
  "command": Command;
  "instanceId": InstanceId;
  "prepared": (boolean);
  "runId": (RunId | (null));
  "sendCommandId": (CommandId | (null));
  [key: string]: unknown;
});

/** InstanceForkParams; `protocol.md` §7.2. */
export type InstanceForkParams = ({
  "nativeBoundary": ForkBoundary;
  "newInstanceId": InstanceId;
  "newSpec": InstanceSpec;
  "sourceInstanceId": InstanceId;
  [key: string]: unknown;
});

export type InstanceId = (string);

/** InstanceLifecycle wire values; `protocol.md` §2.3. */
export type InstanceLifecycle = ("requested" | "preparing" | "starting" | "ready" | "closing" | "exited" | "failed" | "unknown" | "reconciling");

/** InstanceListParams; `protocol.md` §7.2. */
export type InstanceListParams = ({
  "cursor"?: (string | null);
  "limit": (number);
  "workspaceId"?: (WorkspaceId | (null));
  [key: string]: unknown;
});

/** InstanceMode wire values; `protocol.md` §2.3. */
export type InstanceMode = ("native" | "promoted");

/** Explicit human request to create a Claude background attach pane; §7.2. */
export type InstanceOpenTerminalParams = ({
  "allowWake": BoolLiteral_true;
  "backgroundJobId": (string);
  "carrier": PtyCarrier;
  "instanceId": InstanceId;
});

/** InstanceParams; `protocol.md` §7.2. */
export type InstanceParams = ({
  "instanceId": InstanceId;
  [key: string]: unknown;
});

/** InstanceParent; `protocol.md` §2.3. */
export type InstanceParent = ({
  "commandId": CommandId;
  "instanceId": InstanceId;
  "runId": RunId;
  [key: string]: unknown;
});

/** InstanceResumeParams; `protocol.md` §7.2. */
export type InstanceResumeParams = ({
  "expectedPreviousGeneration": U64;
  "instanceId": InstanceId;
  "nativeRef": NativeRef;
  "providerProfileRevision": U64;
  [key: string]: unknown;
});

/** Resource reach of one delegation node; design §2.5.  Scope narrows monotonically down the tree: a child scope must be a subset of its parent's. An *empty* dimension means "no narrowing on this dimension" for the dimension's parent view — a node with every dimension empty is the universe root (the human-seated top coordinator). */
export type InstanceScope = ({
  "hostIds"?: ((HostId)[]);
  "projectIds"?: ((ProjectId)[]);
  "supplyGrants"?: (((string))[]);
  "workspaceIds"?: ((WorkspaceId)[]);
  [key: string]: unknown;
});

/** InstanceSendParams; `protocol.md` §7.2. */
export type InstanceSendParams = ({
  "completionScope": CompletionScope;
  "input": SendInput;
  "instanceId": InstanceId;
  "runId": RunId;
  [key: string]: unknown;
});

/** InstanceSnapshot; `protocol.md` §7.3. */
export type InstanceSnapshot = ({
  "asOfSeq": U64;
  "commands": ((Command)[]);
  "history": HistoryCoverage;
  "instance": Instance;
  "nodes": ((ConversationNode)[]);
  "pendingInteractions": ((Interaction)[]);
  "projectionEpoch": Id;
  "projectionVersion": (string);
  "runs": ((Run)[]);
  [key: string]: unknown;
});

/** InstanceSpec; `protocol.md` §4.1. */
export type InstanceSpec = ({
  "args": (((string))[]);
  "binaryPath"?: (string | null);
  "binaryRef": Id;
  "binarySha256"?: (Digest | (null));
  "carrier": CarrierSpec;
  "completionScope": CompletionScope;
  "cwd": (string);
  "driver": DriverKind;
  "effort"?: (EffortSelection | (null));
  "env": ({
  [key: string]: EnvBinding;
});
  "host": HostId;
  "kind": AgentKind;
  "modelId": (string | null);
  "nativeHome": NativeHome;
  "parent": (InstanceParent | (null));
  "permissionMode": PermissionMode;
  "providerProfile": ProfileRef;
  "requiredCapabilities": ((CapabilityName)[]);
  "schemaVersion": SchemaVersion;
  "settingsOverlay": SettingsOverlay;
  "tui"?: (TuiMode | (null));
  "workspaceId": WorkspaceId;
  "worktree"?: (WorktreeSpec | (null));
  [key: string]: unknown;
});

/** Interaction; `protocol.md` §2.6. */
export type Interaction = ({
  "answer": Knowledge23;
  "answerable": (boolean);
  "blocking": (boolean);
  "carrier": InteractionCarrier;
  "createdAt": Timestamp;
  "deadline": Knowledge9;
  "deadlineSource": DeadlineSource;
  "delivery": DeliveryState;
  "hostId": HostId;
  "id": InteractionId;
  "instanceId": InstanceId;
  "kind": InteractionKind;
  "request": InteractionRequest;
  "requestKey": InteractionRequestKey;
  "requestVersion": U64;
  "resolution": Knowledge24;
  "revision": U64;
  "runId": (RunId | (null));
  "state": InteractionState;
  "updatedAt": Timestamp;
  [key: string]: unknown;
});

/** InteractionAnswer; `protocol.md` §5.4. */
export type InteractionAnswer = (ApprovalAnswer & ({
  "kind": "approval";
  [key: string]: unknown;
}) | QuestionAnswer & ({
  "kind": "question";
  [key: string]: unknown;
}) | PlanReviewAnswer & ({
  "kind": "plan-review";
  [key: string]: unknown;
}) | ElicitationAnswer & ({
  "kind": "elicitation";
  [key: string]: unknown;
}));

/** InteractionAnsweredPayload; `protocol.md` §5.4. */
export type InteractionAnsweredPayload = ({
  "actor": ActorRef;
  "answerCommandId": CommandId;
  "answerRef": Id;
  "delivery": DeliveryState;
  "interactionId": InteractionId;
  "requestVersion": U64;
  [key: string]: unknown;
});

/** InteractionCarrier wire values; `protocol.md` §2.6. */
export type InteractionCarrier = ("claude-control" | "claude-hook" | "harness-hook" | "codex-rpc" | "acp-rpc" | "native-tty" | "unsupported");

/** InteractionExpiredPayload; `protocol.md` §5.4. */
export type InteractionExpiredPayload = ({
  "evidenceEventIds": ((EventId)[]);
  "interactionId": InteractionId;
  "reason": InteractionExpiredReason;
  "requestVersion": U64;
  [key: string]: unknown;
});

/** InteractionExpiredReason wire values; `protocol.md` §5.4. */
export type InteractionExpiredReason = ("deadline" | "native-cancelled" | "generation-ended" | "replaced" | "channel-lost");

export type InteractionId = (string);

/** InteractionKind wire values; `protocol.md` §2.6. */
export type InteractionKind = ("approval" | "question" | "plan-review" | "elicitation");

/** InteractionListParams; `protocol.md` §7.2. */
export type InteractionListParams = ({
  "instanceId": InstanceId;
  "state"?: (InteractionState | (null));
  [key: string]: unknown;
});

/** InteractionParams; `protocol.md` §7.2. */
export type InteractionParams = ({
  "interactionId": InteractionId;
  [key: string]: unknown;
});

/** InteractionRequest; `protocol.md` §5.4. */
export type InteractionRequest = (ApprovalRequest & ({
  "kind": "approval";
  [key: string]: unknown;
}) | QuestionRequest & ({
  "kind": "question";
  [key: string]: unknown;
}) | PlanReviewRequest & ({
  "kind": "plan-review";
  [key: string]: unknown;
}) | ElicitationRequest & ({
  "kind": "elicitation";
  [key: string]: unknown;
}));

/** InteractionRequestKey; `protocol.md` §2.6. */
export type InteractionRequestKey = ({
  "connectionEpoch": Id;
  "native": NativeRequestKey;
  "processGeneration": U64;
  "runGeneration": (U64 | (null));
  [key: string]: unknown;
});

/** InteractionRequestedPayload; `protocol.md` §5.4. */
export type InteractionRequestedPayload = ({
  "interaction": Interaction;
  [key: string]: unknown;
});

/** InteractionResolution; `protocol.md` §2.6. */
export type InteractionResolution = ({
  "eventIds": ((EventId)[]);
  "reason": InteractionResolutionReason;
  [key: string]: unknown;
});

/** InteractionResolutionReason wire values; `protocol.md` §2.6. */
export type InteractionResolutionReason = ("answered" | "native-cleared" | "native-cancelled" | "generation-ended" | "timed-out");

/** InteractionRespondParams; `protocol.md` §6.1. */
export type InteractionRespondParams = ({
  "answer": InteractionAnswer;
  "connectionEpoch": Id;
  "interactionId": InteractionId;
  "processGeneration": U64;
  "requestVersion": U64;
  "runGeneration": (U64 | (null));
  [key: string]: unknown;
});

/** InteractionRespondResult; `protocol.md` §7.2. */
export type InteractionRespondResult = ({
  "command": Command;
  "interaction": Interaction;
  [key: string]: unknown;
});

/** InteractionState wire values; `protocol.md` §2.6. */
export type InteractionState = ("pending" | "answer-committed" | "resolved" | "expired" | "invalidated" | "unknown" | "reconciling");

export type JournalEvent = (Observation | (RegistryEvent & { "instanceId"?: never }));

/** JournalWatermark; `protocol.md` §7.1. */
export type JournalWatermark = ({
  "durableSeq": U64;
  "journalId": Id;
  [key: string]: unknown;
});

/** JsonRpcVersion wire values; `protocol.md` §7.1. */
export type JsonRpcVersion = ("2.0");

/** Explicit knowledge, distinct from an absent relationship; `protocol.md` §1.1. */
export type Knowledge = (({
  "state": "known";
  "value": Digest;
  [key: string]: unknown;
}) | ({
  "evidenceEventIds": ((EventId)[]);
  "reason": (string);
  "state": "unknown";
  [key: string]: unknown;
}) | ({
  "state": "not-applicable";
  [key: string]: unknown;
}));

/** Explicit knowledge, distinct from an absent relationship; `protocol.md` §1.1. */
export type Knowledge10 = (({
  "state": "known";
  "value": NodeReceipt;
  [key: string]: unknown;
}) | ({
  "evidenceEventIds": ((EventId)[]);
  "reason": (string);
  "state": "unknown";
  [key: string]: unknown;
}) | ({
  "state": "not-applicable";
  [key: string]: unknown;
}));

/** Explicit knowledge, distinct from an absent relationship; `protocol.md` §1.1. */
export type Knowledge11 = (({
  "state": "known";
  "value": unknown;
  [key: string]: unknown;
}) | ({
  "evidenceEventIds": ((EventId)[]);
  "reason": (string);
  "state": "unknown";
  [key: string]: unknown;
}) | ({
  "state": "not-applicable";
  [key: string]: unknown;
}));

/** Explicit knowledge, distinct from an absent relationship; `protocol.md` §1.1. */
export type Knowledge12 = (({
  "state": "known";
  "value": ExecutorRef;
  [key: string]: unknown;
}) | ({
  "evidenceEventIds": ((EventId)[]);
  "reason": (string);
  "state": "unknown";
  [key: string]: unknown;
}) | ({
  "state": "not-applicable";
  [key: string]: unknown;
}));

/** Explicit knowledge, distinct from an absent relationship; `protocol.md` §1.1. */
export type Knowledge13 = (({
  "state": "known";
  "value": (number);
  [key: string]: unknown;
}) | ({
  "evidenceEventIds": ((EventId)[]);
  "reason": (string);
  "state": "unknown";
  [key: string]: unknown;
}) | ({
  "state": "not-applicable";
  [key: string]: unknown;
}));

/** Explicit knowledge, distinct from an absent relationship; `protocol.md` §1.1. */
export type Knowledge14 = (({
  "state": "known";
  "value": Platform;
  [key: string]: unknown;
}) | ({
  "evidenceEventIds": ((EventId)[]);
  "reason": (string);
  "state": "unknown";
  [key: string]: unknown;
}) | ({
  "state": "not-applicable";
  [key: string]: unknown;
}));

/** Explicit knowledge, distinct from an absent relationship; `protocol.md` §1.1. */
export type Knowledge15 = (({
  "state": "known";
  "value": Id;
  [key: string]: unknown;
}) | ({
  "evidenceEventIds": ((EventId)[]);
  "reason": (string);
  "state": "unknown";
  [key: string]: unknown;
}) | ({
  "state": "not-applicable";
  [key: string]: unknown;
}));

/** Explicit knowledge, distinct from an absent relationship; `protocol.md` §1.1. */
export type Knowledge16 = (({
  "state": "known";
  "value": RepositoryRef;
  [key: string]: unknown;
}) | ({
  "evidenceEventIds": ((EventId)[]);
  "reason": (string);
  "state": "unknown";
  [key: string]: unknown;
}) | ({
  "state": "not-applicable";
  [key: string]: unknown;
}));

/** Explicit knowledge, distinct from an absent relationship; `protocol.md` §1.1. */
export type Knowledge17 = (({
  "state": "known";
  "value": (boolean);
  [key: string]: unknown;
}) | ({
  "evidenceEventIds": ((EventId)[]);
  "reason": (string);
  "state": "unknown";
  [key: string]: unknown;
}) | ({
  "state": "not-applicable";
  [key: string]: unknown;
}));

/** Explicit knowledge, distinct from an absent relationship; `protocol.md` §1.1. */
export type Knowledge18 = (({
  "state": "known";
  "value": Activity;
  [key: string]: unknown;
}) | ({
  "evidenceEventIds": ((EventId)[]);
  "reason": (string);
  "state": "unknown";
  [key: string]: unknown;
}) | ({
  "state": "not-applicable";
  [key: string]: unknown;
}));

/** Explicit knowledge, distinct from an absent relationship; `protocol.md` §1.1. */
export type Knowledge19 = (({
  "state": "known";
  "value": ProcessExit;
  [key: string]: unknown;
}) | ({
  "evidenceEventIds": ((EventId)[]);
  "reason": (string);
  "state": "unknown";
  [key: string]: unknown;
}) | ({
  "state": "not-applicable";
  [key: string]: unknown;
}));

/** Explicit knowledge, distinct from an absent relationship; `protocol.md` §1.1. */
export type Knowledge2 = (({
  "state": "known";
  "value": (string);
  [key: string]: unknown;
}) | ({
  "evidenceEventIds": ((EventId)[]);
  "reason": (string);
  "state": "unknown";
  [key: string]: unknown;
}) | ({
  "state": "not-applicable";
  [key: string]: unknown;
}));

/** Explicit knowledge, distinct from an absent relationship; `protocol.md` §1.1. */
export type Knowledge20 = (({
  "state": "known";
  "value": TerminalEvidence;
  [key: string]: unknown;
}) | ({
  "evidenceEventIds": ((EventId)[]);
  "reason": (string);
  "state": "unknown";
  [key: string]: unknown;
}) | ({
  "state": "not-applicable";
  [key: string]: unknown;
}));

/** Explicit knowledge, distinct from an absent relationship; `protocol.md` §1.1. */
export type Knowledge21 = (({
  "state": "known";
  "value": RunResult;
  [key: string]: unknown;
}) | ({
  "evidenceEventIds": ((EventId)[]);
  "reason": (string);
  "state": "unknown";
  [key: string]: unknown;
}) | ({
  "state": "not-applicable";
  [key: string]: unknown;
}));

/** Explicit knowledge, distinct from an absent relationship; `protocol.md` §1.1. */
export type Knowledge22 = (({
  "state": "known";
  "value": OutstandingWork;
  [key: string]: unknown;
}) | ({
  "evidenceEventIds": ((EventId)[]);
  "reason": (string);
  "state": "unknown";
  [key: string]: unknown;
}) | ({
  "state": "not-applicable";
  [key: string]: unknown;
}));

/** Explicit knowledge, distinct from an absent relationship; `protocol.md` §1.1. */
export type Knowledge23 = (({
  "state": "known";
  "value": CommittedAnswer;
  [key: string]: unknown;
}) | ({
  "evidenceEventIds": ((EventId)[]);
  "reason": (string);
  "state": "unknown";
  [key: string]: unknown;
}) | ({
  "state": "not-applicable";
  [key: string]: unknown;
}));

/** Explicit knowledge, distinct from an absent relationship; `protocol.md` §1.1. */
export type Knowledge24 = (({
  "state": "known";
  "value": InteractionResolution;
  [key: string]: unknown;
}) | ({
  "evidenceEventIds": ((EventId)[]);
  "reason": (string);
  "state": "unknown";
  [key: string]: unknown;
}) | ({
  "state": "not-applicable";
  [key: string]: unknown;
}));

/** Explicit knowledge, distinct from an absent relationship; `protocol.md` §1.1. */
export type Knowledge25 = (({
  "state": "known";
  "value": Cost;
  [key: string]: unknown;
}) | ({
  "evidenceEventIds": ((EventId)[]);
  "reason": (string);
  "state": "unknown";
  [key: string]: unknown;
}) | ({
  "state": "not-applicable";
  [key: string]: unknown;
}));

/** Explicit knowledge, distinct from an absent relationship; `protocol.md` §1.1. */
export type Knowledge26 = (({
  "state": "known";
  "value": NativeTerminalFrame;
  [key: string]: unknown;
}) | ({
  "evidenceEventIds": ((EventId)[]);
  "reason": (string);
  "state": "unknown";
  [key: string]: unknown;
}) | ({
  "state": "not-applicable";
  [key: string]: unknown;
}));

/** Explicit knowledge, distinct from an absent relationship; `protocol.md` §1.1. */
export type Knowledge3 = (({
  "state": "known";
  "value": U64;
  [key: string]: unknown;
}) | ({
  "evidenceEventIds": ((EventId)[]);
  "reason": (string);
  "state": "unknown";
  [key: string]: unknown;
}) | ({
  "state": "not-applicable";
  [key: string]: unknown;
}));

/** Explicit knowledge, distinct from an absent relationship; `protocol.md` §1.1. */
export type Knowledge4 = (({
  "state": "known";
  "value": TranscriptRef;
  [key: string]: unknown;
}) | ({
  "evidenceEventIds": ((EventId)[]);
  "reason": (string);
  "state": "unknown";
  [key: string]: unknown;
}) | ({
  "state": "not-applicable";
  [key: string]: unknown;
}));

/** Explicit knowledge, distinct from an absent relationship; `protocol.md` §1.1. */
export type Knowledge5 = (({
  "state": "known";
  "value": ProcessIdentity;
  [key: string]: unknown;
}) | ({
  "evidenceEventIds": ((EventId)[]);
  "reason": (string);
  "state": "unknown";
  [key: string]: unknown;
}) | ({
  "state": "not-applicable";
  [key: string]: unknown;
}));

/** Explicit knowledge, distinct from an absent relationship; `protocol.md` §1.1. */
export type Knowledge6 = (({
  "state": "known";
  "value": ForwardIntent;
  [key: string]: unknown;
}) | ({
  "evidenceEventIds": ((EventId)[]);
  "reason": (string);
  "state": "unknown";
  [key: string]: unknown;
}) | ({
  "state": "not-applicable";
  [key: string]: unknown;
}));

/** Explicit knowledge, distinct from an absent relationship; `protocol.md` §1.1. */
export type Knowledge7 = (({
  "state": "known";
  "value": Acceptance;
  [key: string]: unknown;
}) | ({
  "evidenceEventIds": ((EventId)[]);
  "reason": (string);
  "state": "unknown";
  [key: string]: unknown;
}) | ({
  "state": "not-applicable";
  [key: string]: unknown;
}));

/** Explicit knowledge, distinct from an absent relationship; `protocol.md` §1.1. */
export type Knowledge8 = (({
  "state": "known";
  "value": Settlement;
  [key: string]: unknown;
}) | ({
  "evidenceEventIds": ((EventId)[]);
  "reason": (string);
  "state": "unknown";
  [key: string]: unknown;
}) | ({
  "state": "not-applicable";
  [key: string]: unknown;
}));

/** Explicit knowledge, distinct from an absent relationship; `protocol.md` §1.1. */
export type Knowledge9 = (({
  "state": "known";
  "value": Timestamp;
  [key: string]: unknown;
}) | ({
  "evidenceEventIds": ((EventId)[]);
  "reason": (string);
  "state": "unknown";
  [key: string]: unknown;
}) | ({
  "state": "not-applicable";
  [key: string]: unknown;
}));

/** LaunchedBy wire values; `protocol.md` §2.3. */
export type LaunchedBy = ("remuda" | "user");

/** LifecycleEntity; `protocol.md` §5.5. */
export type LifecycleEntity = (({
  "entity": Host;
  "entityType": "host";
  [key: string]: unknown;
}) | ({
  "entity": Workspace;
  "entityType": "workspace";
  [key: string]: unknown;
}) | ({
  "entity": Instance;
  "entityType": "instance";
  [key: string]: unknown;
}) | ({
  "entity": Run;
  "entityType": "run";
  [key: string]: unknown;
}) | ({
  "entity": Command;
  "entityType": "command";
  [key: string]: unknown;
}) | ({
  "entity": Interaction;
  "entityType": "interaction";
  [key: string]: unknown;
}));

/** LifecyclePayload; `protocol.md` §5.5. */
export type LifecyclePayload = (EntityLifecycle & ({
  "type": "entity";
  [key: string]: unknown;
}) | NativeLifecycle & ({
  "type": "native";
  [key: string]: unknown;
}));

/** LifecycleTopic wire values; `protocol.md` §5.5. */
export type LifecycleTopic = ("session" | "turn" | "hook" | "subagent" | "task" | "plan" | "configuration" | "permission" | "diagnostic" | "reconciliation");

/** LiteralEnv; `protocol.md` §4.1. */
export type LiteralEnv = ({
  "value": (string);
  "visibility": EnvVisibility;
  [key: string]: unknown;
});

/** The inherited owner-intent chain carried by every task; design §2.5. */
export type Mandate = ({
  "chain": ((MandateLink)[]);
  [key: string]: unknown;
});

/** One edge of the inherited owner-intent chain.  Every delegation edge attaches the upstream's own words, so a depth-3 node still reads the owner's original instruction rather than a retelling. */
export type MandateLink = ({
  "depth": (number);
  "intent": (string);
  "taskId": TaskId;
  "title": (string);
  [key: string]: unknown;
});

/** MediaBlock; `protocol.md` §5.2. */
export type MediaBlock = ({
  "anchor"?: (number | null);
  "mediaType": (string);
  "name": (string | null);
  "objectId": Id;
  "size"?: (number | null);
  [key: string]: unknown;
});

/** MessageOrigin wire values; `protocol.md` §5.2. */
export type MessageOrigin = ("human" | "injected-skill" | "injected-command-output" | "hook-context" | "tool-result" | "compaction" | "unknown");

/** MessagePayload; `protocol.md` §5.2. */
export type MessagePayload = ({
  "baseRevision": (U64 | (null));
  "blocks": ((ContentBlock)[]);
  "commandId"?: (CommandId | (null));
  "messageId": Id;
  "nativeOrigin": Knowledge2;
  "nodeId": Id;
  "operation": MutationOperation;
  "origin"?: (MessageOrigin | (null));
  "parentToolCallId": (Id | (null));
  "phase": MessagePhase;
  "promptMode"?: (PromptMode | (null));
  "revision": U64;
  "role": MessageRole;
  "status": ContentStatus;
  "targetBlock": (number | null);
  [key: string]: unknown;
});

/** MessagePhase wire values; `protocol.md` §5.2. */
export type MessagePhase = ("input" | "commentary" | "final" | "unknown");

/** MessageRole wire values; `protocol.md` §5.2. */
export type MessageRole = ("user" | "assistant" | "system");

/** MethodCall; `protocol.md` §7.2. */
export type MethodCall = (({
  "method": "runtime.hello";
  "params": HelloParams;
  [key: string]: unknown;
}) | ({
  "method": "runtime.heartbeat";
  "params": HeartbeatParams;
  [key: string]: unknown;
}) | ({
  "method": "host.report";
  "params": HostReportParams;
  [key: string]: unknown;
}) | ({
  "method": "host.get";
  "params": HostParams;
  [key: string]: unknown;
}) | ({
  "method": "host.list";
  "params": PageParams;
  [key: string]: unknown;
}) | ({
  "method": "driver.list";
  "params": HostParams;
  [key: string]: unknown;
}) | ({
  "method": "driver.capabilities";
  "params": DriverCapabilitiesParams;
  [key: string]: unknown;
}) | ({
  "method": "workspace.register";
  "params": CommandEnvelope2;
  [key: string]: unknown;
}) | ({
  "method": "workspace.get";
  "params": WorkspaceParams;
  [key: string]: unknown;
}) | ({
  "method": "workspace.list";
  "params": WorkspaceListParams;
  [key: string]: unknown;
}) | ({
  "method": "worktree.create";
  "params": CommandEnvelope3;
  [key: string]: unknown;
}) | ({
  "method": "worktree.remove";
  "params": CommandEnvelope4;
  [key: string]: unknown;
}) | ({
  "method": "instance.create";
  "params": CommandEnvelope5;
  [key: string]: unknown;
}) | ({
  "method": "instance.attach";
  "params": CommandEnvelope6;
  [key: string]: unknown;
}) | ({
  "method": "instance.open_terminal";
  "params": CommandEnvelope7;
  [key: string]: unknown;
}) | ({
  "method": "instance.resume";
  "params": CommandEnvelope8;
  [key: string]: unknown;
}) | ({
  "method": "instance.send";
  "params": CommandEnvelope9;
  [key: string]: unknown;
}) | ({
  "method": "instance.configure";
  "params": CommandEnvelope10;
  [key: string]: unknown;
}) | ({
  "method": "instance.fork";
  "params": CommandEnvelope11;
  [key: string]: unknown;
}) | ({
  "method": "instance.cancel";
  "params": CommandEnvelope12;
  [key: string]: unknown;
}) | ({
  "method": "instance.close";
  "params": CommandEnvelope13;
  [key: string]: unknown;
}) | ({
  "method": "instance.get";
  "params": InstanceParams;
  [key: string]: unknown;
}) | ({
  "method": "instance.list";
  "params": InstanceListParams;
  [key: string]: unknown;
}) | ({
  "method": "command.get";
  "params": CommandParams;
  [key: string]: unknown;
}) | ({
  "method": "command.list";
  "params": CommandListParams;
  [key: string]: unknown;
}) | ({
  "method": "run.get";
  "params": RunParams;
  [key: string]: unknown;
}) | ({
  "method": "run.list";
  "params": RunListParams;
  [key: string]: unknown;
}) | ({
  "method": "run.wait";
  "params": RunWaitParams;
  [key: string]: unknown;
}) | ({
  "method": "workflow.wait";
  "params": WorkflowWaitParams;
  [key: string]: unknown;
}) | ({
  "method": "interaction.list";
  "params": InteractionListParams;
  [key: string]: unknown;
}) | ({
  "method": "interaction.get";
  "params": InteractionParams;
  [key: string]: unknown;
}) | ({
  "method": "interaction.respond";
  "params": CommandEnvelope14;
  [key: string]: unknown;
}) | ({
  "method": "events.subscribe";
  "params": EventsSubscribeParams;
  [key: string]: unknown;
}) | ({
  "method": "events.read";
  "params": EventsReadParams;
  [key: string]: unknown;
}) | ({
  "method": "events.ack";
  "params": EventsAckParams;
  [key: string]: unknown;
}) | ({
  "method": "events.unsubscribe";
  "params": SubscriptionParams;
  [key: string]: unknown;
}) | ({
  "method": "reconcile.instance";
  "params": ReconcileInstanceParams;
  [key: string]: unknown;
}) | ({
  "method": "tty.attach";
  "params": TtyAttachParams;
  [key: string]: unknown;
}) | ({
  "method": "tty.detach";
  "params": TtyDetachParams;
  [key: string]: unknown;
}) | ({
  "method": "tty.write";
  "params": CommandEnvelope15;
  [key: string]: unknown;
}) | ({
  "method": "tty.resize";
  "params": TtyResizeParams;
  [key: string]: unknown;
}) | ({
  "method": "object.stat";
  "params": ObjectParams;
  [key: string]: unknown;
}) | ({
  "method": "object.read";
  "params": ObjectReadParams;
  [key: string]: unknown;
}) | ({
  "method": "object.prepare";
  "params": ObjectPrepareParams;
  [key: string]: unknown;
}) | ({
  "method": "object.write";
  "params": ObjectWriteParams;
  [key: string]: unknown;
}) | ({
  "method": "object.commit";
  "params": ObjectCommitParams;
  [key: string]: unknown;
}));

/** MethodName wire values; `protocol.md` §7.2. */
export type MethodName = ("runtime.hello" | "runtime.heartbeat" | "host.report" | "host.get" | "host.list" | "driver.list" | "driver.capabilities" | "workspace.register" | "workspace.get" | "workspace.list" | "worktree.create" | "worktree.remove" | "instance.create" | "instance.attach" | "instance.open_terminal" | "instance.resume" | "instance.send" | "instance.configure" | "instance.fork" | "instance.cancel" | "instance.close" | "instance.get" | "instance.list" | "command.get" | "command.list" | "run.get" | "run.list" | "run.wait" | "workflow.wait" | "interaction.list" | "interaction.get" | "interaction.respond" | "events.subscribe" | "events.read" | "events.ack" | "events.unsubscribe" | "reconcile.instance" | "tty.attach" | "tty.detach" | "tty.write" | "tty.resize" | "object.stat" | "object.read" | "object.prepare" | "object.write" | "object.commit");

/** The model list a session can actually switch to, with its provenance. */
export type ModelCatalogInfo = ({
  "models": (((string))[]);
  "observedAt": Timestamp;
  "source": ModelListSource;
  [key: string]: unknown;
});

/** Model capability class used by `TaskSpec.minClass` and the catalog; §4.2. */
export type ModelClass = ("cheap" | "workhorse" | "frontier");

/** ModelEffective wire values; `protocol.md` §3.1. */
export type ModelEffective = ("next-turn");

/** Where the session's model list was discovered. */
export type ModelListSource = ("gateway-discovery" | "settings" | "builtin");

/** ModelPayload; `protocol.md` §5.1 (D-028 §9.1 model sync).  Emitted when the effective model is observed: an assistant record's `message.model` or a `/model` command's `<local-command-stdout>` verdict. Unchanged ids are deduped by the emitting driver, so the Hub only sees edges. The UI renders the current model from this observation — never from the requested switch. */
export type ModelPayload = ({
  "catalog"?: (ModelCatalogInfo | (null));
  "effective": EffectiveModel;
  "raw"?: (string | null);
  "requested"?: (string | null);
  [key: string]: unknown;
});

/** Project speaks model roles, not concrete model ids; design §3.2. */
export type ModelRoles = ({
  "cheap"?: (string | null);
  "frontier"?: (string | null);
  "reviewer"?: (string | null);
  "workhorse"?: (string | null);
  [key: string]: unknown;
});

/** ModelSwitchInput; `protocol.md` §3.1. */
export type ModelSwitchInput = ({
  "effective": ModelEffective;
  "effort"?: (string | null);
  "modelId": (string);
  "permissionMode"?: (string | null);
  [key: string]: unknown;
});

/** MutationOperation wire values; `protocol.md` §5.2. */
export type MutationOperation = ("open" | "append" | "replace" | "close");

/** NamedPermissions; `protocol.md` §4.1. */
export type NamedPermissions = ({
  "permissions": (string);
});

/** NativeHome; `protocol.md` §4.1. */
export type NativeHome = ({
  "mode": NativeHomeMode;
  "storeId": Id;
  [key: string]: unknown;
});

/** NativeHomeMode wire values; `protocol.md` §4.1. */
export type NativeHomeMode = ("registered");

/** NativeLifecycle; `protocol.md` §5.5. */
export type NativeLifecycle = ({
  "affectsCompletion": (boolean);
  "dataRef": (Id | (null));
  "nativeId": Knowledge2;
  "nativeName": (string);
  "relatedIds": ({
  [key: string]: (string);
});
  "severity": Severity;
  "status": Knowledge2;
  "topic": LifecycleTopic;
  [key: string]: unknown;
});

/** NativeLocator; `protocol.md` §5.5. */
export type NativeLocator = ({
  "nativeUri": (string);
  [key: string]: unknown;
});

/** NativeRef; `protocol.md` §1.3. */
export type NativeRef = ({
  "acp"?: (AcpRef | (null));
  "agy"?: (AgyRef | (null));
  "capabilities"?: ((RuntimeCapability)[]);
  "claude"?: (ClaudeRef | (null));
  "claudeBg"?: (ClaudeBgRef | (null));
  "codex"?: (CodexRef | (null));
  "herdr"?: (HerdrRef | (null));
  "hostId": HostId;
  "kind": AgentKind;
  "nativeStoreId": Id;
  "sessionId": Knowledge2;
  "signalTier"?: (SignalTier | (null));
  "transcript": Knowledge4;
  [key: string]: unknown;
});

/** Correlation of a native RPC, blocking hook, or absent request; `protocol.md` §1.3. */
export type NativeRequestKey = (({
  "type": "rpc";
  "value": (string);
  "valueType": NativeRequestValueType;
  [key: string]: unknown;
}) | ({
  "invocationId": Id;
  "type": "hook";
  [key: string]: unknown;
}) | ({
  "type": "none";
  [key: string]: unknown;
}));

/** NativeRequestValueType wire values; `protocol.md` §1.3. */
export type NativeRequestValueType = ("string" | "number");

/** NativeTerminalFrame; `protocol.md` §5.5. */
export type NativeTerminalFrame = ({
  "full": (boolean);
  "height": (number);
  "seq": U64;
  "width": (number);
  [key: string]: unknown;
});

/** NativeTurn; `protocol.md` §2.4. */
export type NativeTurn = ({
  "id": (string);
  "resultIndex": Knowledge3;
  "source": (string);
  [key: string]: unknown;
});

/** NodeMutation; `protocol.md` §5.2. */
export type NodeMutation = ({
  "baseRevision": (U64 | (null));
  "nodeId": Id;
  "operation": MutationOperation;
  "revision": U64;
  [key: string]: unknown;
});

/** NodeReceipt; `protocol.md` §2.5. */
export type NodeReceipt = ({
  "ledgerRevision": U64;
  "nodeEpoch": Id;
  [key: string]: unknown;
});

export type NonEmpty_AnyValue = ([(unknown), ...(unknown)[]]);

export type NonEmpty_JournalEvent = ([(JournalEvent), ...(JournalEvent)[]]);

/** NotificationBody; `protocol.md` §7.3. */
export type NotificationBody = (({
  "method": "events.batch";
  "params": EventsBatch;
  [key: string]: unknown;
}));

/** NotificationName wire values; `protocol.md` §7.3. */
export type NotificationName = ("events.batch");

/** ObjectCommitParams; `protocol.md` §7.2. */
export type ObjectCommitParams = ({
  "digest": Digest;
  "uploadId": Id;
  [key: string]: unknown;
});

/** ObjectCommitResult; `protocol.md` §7.2. */
export type ObjectCommitResult = ({
  "digest": Digest;
  "objectId": Id;
  "sizeBytes": U64;
  [key: string]: unknown;
});

/** ObjectMetadata; `protocol.md` §7.2. */
export type ObjectMetadata = ({
  "digest": Digest;
  "mediaType": (string);
  "objectId": Id;
  "sizeBytes": U64;
  [key: string]: unknown;
});

/** ObjectParams; `protocol.md` §7.2. */
export type ObjectParams = ({
  "objectId": Id;
  [key: string]: unknown;
});

/** ObjectPrepareParams; `protocol.md` §7.2. */
export type ObjectPrepareParams = ({
  "digest": Digest;
  "hostId": HostId;
  "mediaType": (string);
  "objectId": Id;
  "purpose": ObjectPurpose;
  "sizeBytes": U64;
  "workspaceId": WorkspaceId;
  [key: string]: unknown;
});

/** ObjectPrepareResult; `protocol.md` §7.2. */
export type ObjectPrepareResult = ({
  "expiresAt": Timestamp;
  "maxChunkBytes": (number);
  "nextOffset": U64;
  "uploadId": Id;
  [key: string]: unknown;
});

/** ObjectPurpose wire values; `protocol.md` §7.2. */
export type ObjectPurpose = ("input" | "settings" | "answer");

/** ObjectReadParams; `protocol.md` §7.2. */
export type ObjectReadParams = ({
  "expectedDigest": Digest;
  "length": U64;
  "objectId": Id;
  "offset": U64;
  [key: string]: unknown;
});

/** ObjectReadResult; `protocol.md` §7.2. */
export type ObjectReadResult = ({
  "digest": Digest;
  "length": U64;
  "objectId": Id;
  "offset": U64;
  "streamId": Id;
  [key: string]: unknown;
});

/** ObjectWriteParams; `protocol.md` §7.2. */
export type ObjectWriteParams = ({
  "chunkDigest": Digest;
  "dataBase64": (string);
  "offset": U64;
  "uploadId": Id;
  [key: string]: unknown;
});

/** ObjectWriteResult; `protocol.md` §7.2. */
export type ObjectWriteResult = ({
  "nextOffset": U64;
  [key: string]: unknown;
});

/** Observation; `protocol.md` §5.1. */
export type Observation = (({
  "kind": "message";
  "payload": MessagePayload;
  [key: string]: unknown;
}) | ({
  "kind": "thought";
  "payload": ThoughtPayload;
  [key: string]: unknown;
}) | ({
  "kind": "tool_call";
  "payload": ToolCallPayload;
  [key: string]: unknown;
}) | ({
  "kind": "tool_result";
  "payload": ToolResultPayload;
  [key: string]: unknown;
}) | ({
  "kind": "interaction.requested";
  "payload": InteractionRequestedPayload;
  [key: string]: unknown;
}) | ({
  "kind": "interaction.answered";
  "payload": InteractionAnsweredPayload;
  [key: string]: unknown;
}) | ({
  "kind": "interaction.expired";
  "payload": InteractionExpiredPayload;
  [key: string]: unknown;
}) | ({
  "kind": "workflow.run";
  "payload": WorkflowRunPayload;
  [key: string]: unknown;
}) | ({
  "kind": "workflow.phase";
  "payload": WorkflowPhasePayload;
  [key: string]: unknown;
}) | ({
  "kind": "workflow.member";
  "payload": WorkflowMemberPayload;
  [key: string]: unknown;
}) | ({
  "kind": "lifecycle";
  "payload": LifecyclePayload;
  [key: string]: unknown;
}) | ({
  "kind": "usage";
  "payload": UsagePayload;
  [key: string]: unknown;
}) | ({
  "kind": "artifact";
  "payload": ArtifactPayload;
  [key: string]: unknown;
}) | ({
  "kind": "effort";
  "payload": EffortPayload;
  [key: string]: unknown;
}) | ({
  "kind": "model";
  "payload": ModelPayload;
  [key: string]: unknown;
}) | ({
  "kind": "permission";
  "payload": PermissionPayload;
  [key: string]: unknown;
}) | ({
  "kind": "raw_tty";
  "payload": RawTtyPayload;
  [key: string]: unknown;
}) | ({
  "kind": "opaque";
  "payload": OpaquePayload;
  [key: string]: unknown;
})) & ({
  "completeness": Completeness;
  "eventId": EventId;
  "evidenceEventIds": ((EventId)[]);
  "hostId": HostId;
  "instanceId": InstanceId;
  "journalId": Id;
  "nativeAt": Knowledge9;
  "observedAt": Timestamp;
  "processGeneration": U64;
  "rawRef": (RawRef | (null));
  "runGeneration": (U64 | (null));
  "runId": (RunId | (null));
  "schemaVersion": SchemaVersion;
  "seq": U64;
  "source": ObservationSource;
  [key: string]: unknown;
});

/** ObservationKind wire values; `protocol.md` §5.1. */
export type ObservationKind = ("message" | "thought" | "tool_call" | "tool_result" | "interaction.requested" | "interaction.answered" | "interaction.expired" | "workflow.run" | "workflow.phase" | "workflow.member" | "lifecycle" | "usage" | "artifact" | "effort" | "model" | "permission" | "raw_tty" | "opaque");

/** ObservationPayload; `protocol.md` §5.1. */
export type ObservationPayload = (({
  "kind": "message";
  "payload": MessagePayload;
  [key: string]: unknown;
}) | ({
  "kind": "thought";
  "payload": ThoughtPayload;
  [key: string]: unknown;
}) | ({
  "kind": "tool_call";
  "payload": ToolCallPayload;
  [key: string]: unknown;
}) | ({
  "kind": "tool_result";
  "payload": ToolResultPayload;
  [key: string]: unknown;
}) | ({
  "kind": "interaction.requested";
  "payload": InteractionRequestedPayload;
  [key: string]: unknown;
}) | ({
  "kind": "interaction.answered";
  "payload": InteractionAnsweredPayload;
  [key: string]: unknown;
}) | ({
  "kind": "interaction.expired";
  "payload": InteractionExpiredPayload;
  [key: string]: unknown;
}) | ({
  "kind": "workflow.run";
  "payload": WorkflowRunPayload;
  [key: string]: unknown;
}) | ({
  "kind": "workflow.phase";
  "payload": WorkflowPhasePayload;
  [key: string]: unknown;
}) | ({
  "kind": "workflow.member";
  "payload": WorkflowMemberPayload;
  [key: string]: unknown;
}) | ({
  "kind": "lifecycle";
  "payload": LifecyclePayload;
  [key: string]: unknown;
}) | ({
  "kind": "usage";
  "payload": UsagePayload;
  [key: string]: unknown;
}) | ({
  "kind": "artifact";
  "payload": ArtifactPayload;
  [key: string]: unknown;
}) | ({
  "kind": "effort";
  "payload": EffortPayload;
  [key: string]: unknown;
}) | ({
  "kind": "model";
  "payload": ModelPayload;
  [key: string]: unknown;
}) | ({
  "kind": "permission";
  "payload": PermissionPayload;
  [key: string]: unknown;
}) | ({
  "kind": "raw_tty";
  "payload": RawTtyPayload;
  [key: string]: unknown;
}) | ({
  "kind": "opaque";
  "payload": OpaquePayload;
  [key: string]: unknown;
}));

/** ObservationSource; `protocol.md` §5.1. */
export type ObservationSource = ({
  "adapterVersion": (string);
  "channel": SourceChannel;
  "delivery": SourceDelivery;
  "driverKind": DriverKind;
  "driverVersion": (string);
  "nativeAgentId": Knowledge2;
  "nativeEventId": Knowledge2;
  "nativeItemId": Knowledge2;
  "nativeRequestId": NativeRequestKey;
  "nativeSessionId": Knowledge2;
  "nativeTurnId": Knowledge2;
  "sourceCursor": SourceCursor;
  [key: string]: unknown;
});

/** An effort level as read off an assistant transcript record. */
export type ObservedEffort = ({
  "name": EffortName;
  "ultracode": (boolean | null);
  [key: string]: unknown;
});

/** A model id as read off a transcript (verdict or assistant record). */
export type ObservedModel = ({
  "id": (string);
  [key: string]: unknown;
});

/** One observed Codex `account/rateLimits/updated` frame, normalized; §4.2.  The Node does not relay these frames yet (r-p6); the REST `…/supply/events` path accepts this shape so the loop is wired end to end. */
export type ObservedRateLimits = ({
  "windows": ((RateLimitWindow)[]);
  [key: string]: unknown;
});

/** OpaqueBlock; `protocol.md` §5.2. */
export type OpaqueBlock = ({
  "nativeType": (string);
  "rawRef": RawRef;
  [key: string]: unknown;
});

/** OpaqueImpact wire values; `protocol.md` §5.5. */
export type OpaqueImpact = ("presentation" | "control" | "terminal");

/** OpaquePayload; `protocol.md` §5.5. */
export type OpaquePayload = ({
  "affects": ((OpaqueImpact)[]);
  "nativeType": (string);
  "rawRef": RawRef;
  "reason": OpaqueReason;
  "summary": (string | null);
  [key: string]: unknown;
});

/** OpaqueReason wire values; `protocol.md` §5.5. */
export type OpaqueReason = ("unknown-type" | "unknown-version" | "malformed" | "unsupported-extension" | "unmapped-fields" | "screen-snapshot");

/** OutstandingWork; `protocol.md` §2.4. */
export type OutstandingWork = ({
  "childRunIds": ((RunId)[]);
  "detachedTaskIds": (((string))[]);
  "workflowIds": ((Id)[]);
  [key: string]: unknown;
});

/** Ownership wire values; `protocol.md` §2.3. */
export type Ownership = ("managed" | "adopted-control" | "observed-only");

/** Page; `protocol.md` §7.2. */
export type Page = ({
  "items": ((unknown)[]);
  "nextCursor": (string | null);
  [key: string]: unknown;
});

/** PageParams; `protocol.md` §7.2. */
export type PageParams = ({
  "cursor"?: (string | null);
  "limit": (number);
  [key: string]: unknown;
});

/** Parentage wire values; `protocol.md` §2.4. */
export type Parentage = ("known-root" | "linked" | "unknown");

/** PathStyle wire values; `protocol.md` §2.1. */
export type PathStyle = ("posix" | "windows");

/** Effective permission mode, read back from the native TUI status line and the transcript's `permission-mode` records.  Mirrors [`EffortEffective`]: this is the *observed* mode, never the requested one. `mode` carries the protocol wire spelling for every harness (Claude's TUI/transcript spelling `default` is normalized to `manual` by the observing driver). */
export type PermissionEffective = ({
  "mode": (string);
  "observedAt": Timestamp;
  "source": PermissionSource;
  [key: string]: unknown;
});

/** PermissionMode; `protocol.md` §4.1. */
export type PermissionMode = (ClaudePermission & ({
  "kind": "claude";
  [key: string]: unknown;
}) | CodexPermission & ({
  "kind": "codex";
  [key: string]: unknown;
}) | GrokPermission & ({
  "kind": "grok";
  [key: string]: unknown;
}) | AgyPermission & ({
  "kind": "agy";
  [key: string]: unknown;
}) | GenericPermission & ({
  "kind": "generic";
  [key: string]: unknown;
}));

/** Emitted whenever the effective permission mode is observed — the native TUI status line and the transcript's `permission-mode` records agree. Unchanged values are deduped by the observing driver, so the Hub only sees edges. The UI renders from this observation — never from the requested mode. */
export type PermissionPayload = ({
  "effective": PermissionEffective;
  "raw"?: (string | null);
  "requested"?: (string | null);
  [key: string]: unknown;
});

/** PermissionSource wire values; `protocol.md` §9.1. */
export type PermissionSource = ("launch" | "slash" | "remuda" | "unknown");

/** One placement-ledger row; design §2.2 ⑥/§5.6.  `reasons` / `rejected` are both the audit trail and the future bot card body — the two never diverge because there is no second representation. */
export type PlacementLedgerRow = ({
  "branch"?: (string | null);
  "createdAt": Timestamp;
  "createdBy": (string);
  "harness"?: (string | null);
  "hostId"?: (HostId | (null));
  "id": Id;
  "instanceId"?: (InstanceId | (null));
  "kind": (string);
  "model"?: (string | null);
  "projectId": ProjectId;
  "reasons"?: (((string))[]);
  "rejected"?: ((PlacementRejection)[]);
  "taskId": TaskId;
  [key: string]: unknown;
});

/** One rejected candidate and why; design §4.4/§5.6. */
export type PlacementRejection = ({
  "candidate": (string);
  "reason": (string);
  [key: string]: unknown;
});

/** PlanReviewAnswer; `protocol.md` §5.4. */
export type PlanReviewAnswer = ({
  "feedback": (string | null);
  "optionId": (string);
  "planDigest": Digest;
  "planRevision": U64;
  [key: string]: unknown;
});

/** PlanReviewRequest; `protocol.md` §5.4. */
export type PlanReviewRequest = ({
  "allowFeedback": (boolean);
  "options": ((DecisionOption)[]);
  "planDigest": Digest;
  "planRef": Id;
  "planRevision": U64;
  "title": (string);
  [key: string]: unknown;
});

/** Platform; `protocol.md` §2.1. */
export type Platform = ({
  "arch": (string);
  "os": (string);
  "pathStyle": PathStyle;
  [key: string]: unknown;
});

/** ProcessExit; `protocol.md` §2.3. */
export type ProcessExit = ({
  "code": (number | null);
  "observedAt": Timestamp;
  "signal": (string | null);
  [key: string]: unknown;
});

/** ProcessIdentity; `protocol.md` §1.3. */
export type ProcessIdentity = ({
  "birthId": (string);
  "pid": (number);
  "supervisorId": Id;
  [key: string]: unknown;
});

/** ProcessRef; `protocol.md` §1.3. */
export type ProcessRef = ({
  "connectionEpoch": Id;
  "processGeneration": U64;
  "processIdentity": Knowledge5;
  [key: string]: unknown;
});

/** ProfileRef; `protocol.md` §4.1. */
export type ProfileRef = ({
  "id": Id;
  "revision": U64;
  [key: string]: unknown;
});

/** Thin authoritative Hub Project entity; design §3.1–§3.2. */
export type Project = ({
  "branchPattern": (string);
  "briefRef": (string);
  "createdAt": Timestamp;
  "defaultBaseBranch": (string);
  "defaultEffort"?: (string | null);
  "gate": ProjectGate;
  "homeHost"?: (HostId | (null));
  "hosts"?: ((ProjectHostQuota)[]);
  "id": ProjectId;
  "members"?: ((ProjectMember)[]);
  "modelRoles": ModelRoles;
  "name": (string);
  "permissionPosture"?: (string | null);
  "placement": ProjectPlacement;
  "policy": ProjectPolicy;
  "provider": ProjectProviderRef;
  "repoRemote"?: (string | null);
  "revision": U64;
  "updatedAt": Timestamp;
  [key: string]: unknown;
});

/** Configurable policy. Owner-tunable; the first four delegate-tree limits implement design §2.5 ⑤ (depth/fan-out are policy, not schema). */
export type ProjectConfigurablePolicy = ({
  "allowMultipleDispatchers": (boolean);
  "completionLine": (string);
  "coordinatorFanOut": (number);
  "defaultPlacement": (string);
  "maxConcurrentWorkers": (number);
  "maxDelegationDepth": (number);
  "maxFanOutPerTask": (number);
  "nudgeThrottleMins": (number);
  "stallThresholdMins": (number);
  [key: string]: unknown;
});

/** Enforced policy switches. All default on; agents may never change them (D-031 — read-only for coordinators, change requires escalation). */
export type ProjectEnforcedPolicy = ({
  "allocatedPortBlocks": (boolean);
  "casAncestorCheck": (boolean);
  "gateBeforeLand": (boolean);
  "noDeployScripts": (boolean);
  "noOsSettingsChanges": (boolean);
  "noTunnelTools": (boolean);
  "oneWorktreePerWorker": (boolean);
  "reclaimDiskOnRetire": (boolean);
  "secretsNeverInBriefs": (boolean);
  "workersNeverPush": (boolean);
  [key: string]: unknown;
});

/** Gate configuration; design §3.2 (placeholder, consumed by r-mergequeue). */
export type ProjectGate = ({
  "affected": (boolean);
  "command"?: (string | null);
  "landSerialization"?: (string | null);
  "lanes"?: ((ProjectGateLane)[]);
  "mandatorySteps"?: (((string))[]);
  "web"?: (string | null);
  [key: string]: unknown;
});

/** One gate lane; batch 6 enforces these via the Hub gate queue and the Node lane runner (`gate.run`). Lane environment is set once on the Project and sent verbatim on every run — never re-typed per gate. */
export type ProjectGateLane = ({
  "env": ({
  [key: string]: (string);
});
  "hostId": HostId;
  "id": (string);
  "lockPath"?: (string | null);
  "ports"?: (string | null);
  "pwEndpoint"?: (string | null);
  "remote"?: (string | null);
  "repoPath": (string);
  "targetDir": (string);
  "toolchainPath"?: (string | null);
  [key: string]: unknown;
});

/** Project-side capacity view of one host with capacity; design §3.2. */
export type ProjectHostQuota = ({
  "diskBudgetGb"?: (number | null);
  "hostId": HostId;
  "latencyClass"?: (string | null);
  "maxBuilding"?: (number | null);
  "maxInstances"?: (number | null);
  "portBlocks"?: (((string))[]);
  "requires"?: (((string))[]);
  [key: string]: unknown;
});

export type ProjectId = (string);

/** Launch-relevant defaults folded into an instance create; design §3.2 / §6.  Fold priority is explicit (request body) > project > host > global, so every field here only fills a key the request did not name. */
export type ProjectLaunchDefaults = ({
  "agent"?: (string | null);
  "delegation"?: (string | null);
  "driver"?: (string | null);
  "effort"?: (string | null);
  "launchArgs"?: (((string))[]);
  "model"?: (string | null);
  "permissionPosture"?: (string | null);
  "providerProfileId"?: (string | null);
  [key: string]: unknown;
});

/** One `(hostId, workspaceId)` pair — the same key Space uses (D-024). */
export type ProjectMember = ({
  "hostId": HostId;
  "role": (string);
  "workspaceId": WorkspaceId;
  [key: string]: unknown;
});

/** Project placement defaults. */
export type ProjectPlacement = ({
  "default": (string);
  "hostIds"?: ((HostId)[]);
  "labels"?: (((string))[]);
  [key: string]: unknown;
});

/** Full project policy envelope; design §3.2. */
export type ProjectPolicy = ({
  "configurable": ProjectConfigurablePolicy;
  "enforced": ProjectEnforcedPolicy;
  [key: string]: unknown;
});

/** Provider reference (no secret — projects hold only a profileId; design §3.2). */
export type ProjectProviderRef = ({
  "delegation"?: (string | null);
  "profileId"?: (string | null);
  [key: string]: unknown;
});

/** PromptInput; `protocol.md` §3.1. */
export type PromptInput = ({
  "blocks": ((ContentBlock)[]);
  "mode": PromptMode;
  "nativeClientMessageId": (string);
  "origin": InputOrigin;
  [key: string]: unknown;
});

/** PromptMode wire values; `protocol.md` §3.1. */
export type PromptMode = ("new-turn" | "steer" | "queue");

/** ProtocolRange; `protocol.md` §7.1. */
export type ProtocolRange = ({
  "major": (number);
  "maxMinor": (number);
  "minMinor": (number);
  [key: string]: unknown;
});

/** ProtocolVersion; `protocol.md` §7.1. */
export type ProtocolVersion = ({
  "major": (number);
  "minor": (number);
  [key: string]: unknown;
});

/** ProviderIngress wire values; `protocol.md` §4.1. */
export type ProviderIngress = ("anthropic-messages" | "openai-responses" | "openai-chat" | "gemini-native" | "native-login");

/** Launch overlay the Node writes as Claude `--settings`. The token stays off this type. */
export type ProviderOverlaySpec = ({
  "baseUrl": (string);
  "headers": ({
  [key: string]: (string);
});
  "kind": ProviderProfileKind;
  "model": (string);
  "profileId": Id;
  "scope"?: (string);
  [key: string]: unknown;
});

/** Operator-configured provider profile (Hub registry). `protocol.md` §4.4 / D-012.  GET never includes the auth token; only [`ProviderSecretView`]. */
export type ProviderProfile = ({
  "baseUrl": (string);
  "createdAt": Timestamp;
  "defaultGateway": (boolean);
  "defaultModel": (string | null);
  "headers": ({
  [key: string]: (string);
});
  "id": Id;
  "kind": ProviderProfileKind;
  "models": (((string))[]);
  "name": (string);
  "revision": U64;
  "scope"?: (string);
  "secret": ProviderSecretView;
  "updatedAt": Timestamp;
  [key: string]: unknown;
});

/** ProviderProfileKind wire values; `protocol.md` §4.4. */
export type ProviderProfileKind = ("gateway" | "direct");

/** Public secret metadata for a stored ProviderProfile. The token is never on this type. */
export type ProviderSecretView = ({
  "fingerprint": (string | null);
  "last4": (string | null);
  "present": (boolean);
  [key: string]: unknown;
});

/** ProviderSelection; `protocol.md` §4.1. */
export type ProviderSelection = ({
  "credentialRef": (Id | (null));
  "credentialVersion": (U64 | (null));
  "endpointId": Id;
  "ingress": ProviderIngress;
  "modelRequested": (string);
  "modelResolved": Knowledge2;
  "profileId": Id;
  "profileRevision": U64;
  "selectionReason": SelectionReason;
  [key: string]: unknown;
});

/** PtyBackend wire values; `protocol.md` §4.1. */
export type PtyBackend = ("herdr");

/** Herdr-backed PTY launch, pinned before dispatch; `protocol.md` §4.1. */
export type PtyCarrier = ({
  "backend": PtyBackend;
  "server": HerdrServer;
  "session": (string);
});

/** QuestionAnswer; `protocol.md` §5.4. */
export type QuestionAnswer = ({
  "answers": ({
  [key: string]: QuestionFieldAnswer;
});
  [key: string]: unknown;
});

/** QuestionField; `protocol.md` §5.4. */
export type QuestionField = ({
  "allowFreeText": (boolean);
  "description": (string | null);
  "id": (string);
  "input": QuestionInput;
  "options": ((QuestionOption)[]);
  "required": (boolean);
  "sensitive": (boolean);
  "title": (string);
  [key: string]: unknown;
});

/** QuestionFieldAnswer; `protocol.md` §5.4. */
export type QuestionFieldAnswer = ({
  "optionIds": (((string))[]);
  "text": (string | null);
  [key: string]: unknown;
});

/** QuestionInput wire values; `protocol.md` §5.4. */
export type QuestionInput = ("text" | "single-select" | "multi-select");

/** QuestionOption; `protocol.md` §5.4. */
export type QuestionOption = ({
  "description": (string | null);
  "id": (string);
  "label": (string);
  [key: string]: unknown;
});

/** QuestionRequest; `protocol.md` §5.4. */
export type QuestionRequest = ({
  "fields": ((QuestionField)[]);
  "title": (string);
  [key: string]: unknown;
});

/** One rate-limit window — the Codex `RateLimitWindow` vocabulary (§4.2).  `appliesTo` is the field that makes fallback correct: `["*"]` is an account/session/weekly bucket a model switch cannot escape; `["<family>"]` is a family bucket a sibling family can dodge (§4.6). */
export type RateLimitWindow = ({
  "appliesTo": (((string))[]);
  "backoffAttempts"?: (number);
  "cooldownUntil"?: (number | null);
  "id": (string);
  "limit"?: (U64 | (null));
  "observedAt"?: (number | null);
  "resetsAt"?: (number | null);
  "source": WindowSource;
  "usedPercent"?: (number | null);
  "windowDurationMins"?: (number | null);
  [key: string]: unknown;
});

/** RawRef; `protocol.md` §5.1. */
export type RawRef = ({
  "digest": Digest;
  "length": U64;
  "mediaType": (string);
  "objectId": Id;
  "offset": U64;
  "redaction": Redaction;
  [key: string]: unknown;
});

/** RawTtyPayload; `protocol.md` §5.5. */
export type RawTtyPayload = (TtyOutput & ({
  "direction": "output";
  [key: string]: unknown;
}) | TtyInput & ({
  "direction": "input";
  [key: string]: unknown;
}) | TtyResize & ({
  "direction": "resize";
  [key: string]: unknown;
}));

/** ReconcileInstanceParams; `protocol.md` §7.5. */
export type ReconcileInstanceParams = ({
  "commandIds": ((CommandId)[]);
  "expectedGeneration": U64;
  "instanceId": InstanceId;
  [key: string]: unknown;
});

/** ReconcileInstanceResult; `protocol.md` §7.5. */
export type ReconcileInstanceResult = ({
  "evidenceEventIds": ((EventId)[]);
  "state": InstanceLifecycle;
  "unresolvedCommandIds": ((CommandId)[]);
  [key: string]: unknown;
});

/** Redaction wire values; `protocol.md` §5.1. */
export type Redaction = ("none" | "derived-redacted" | "unavailable");

/** RegistryEvent; `protocol.md` §5.5. */
export type RegistryEvent = ({
  "eventId": EventId;
  "hostId": HostId;
  "journalId": Id;
  "kind": RegistryKind;
  "observedAt": Timestamp;
  "payload": LifecyclePayload;
  "schemaVersion": SchemaVersion;
  "seq": U64;
  [key: string]: unknown;
});

/** RegistryKind wire values; `protocol.md` §5.5. */
export type RegistryKind = ("lifecycle");

/** RegistrySnapshot; `protocol.md` §7.3. */
export type RegistrySnapshot = ({
  "asOfSeq": U64;
  "commands": ((Command)[]);
  "history": HistoryCoverage;
  "hosts": ((Host)[]);
  "projectionEpoch": Id;
  "projectionVersion": (string);
  "workspaces": ((Workspace)[]);
  [key: string]: unknown;
});

/** RepositoryRef; `protocol.md` §2.2. */
export type RepositoryRef = ({
  "gitCommonDir": (string);
  "headOid": (string);
  "repositoryId": Id;
  [key: string]: unknown;
});

/** ResolutionState wire values; `protocol.md` §2.5. */
export type ResolutionState = ("clear" | "unknown" | "reconciling");

/** ResourceBlock; `protocol.md` §5.2. */
export type ResourceBlock = ({
  "mediaType": Knowledge2;
  "objectId": (Id | (null));
  "uri": (string);
  [key: string]: unknown;
});

/** ResultStage wire values; `protocol.md` §5.2. */
export type ResultStage = ("partial" | "final");

/** ResumeCursor; `protocol.md` §7.1. */
export type ResumeCursor = ({
  "afterSeq": U64;
  "journalId": Id;
  [key: string]: unknown;
});

/** RetryAction wire values; `protocol.md` §9.1. */
export type RetryAction = ("never" | "read-only" | "same-command-query" | "after-reconciliation" | "new-command");

/** RpcError; `protocol.md` §9.1. */
export type RpcError = ({
  "code": (number);
  "data"?: (RpcErrorData | (null));
  "message": (string);
  [key: string]: unknown;
});

/** RpcErrorData; `protocol.md` §9.1. */
export type RpcErrorData = ({
  "code": ErrorCode;
  "details": ErrorDetails;
  "execution": ExecutionState;
  "retry": RetryAction;
  [key: string]: unknown;
});

/** RpcFailure; `protocol.md` §7.1. */
export type RpcFailure = ({
  "error": RpcError;
  "id": (string);
  "jsonrpc": JsonRpcVersion;
});

/** RpcNotification; `protocol.md` §7.3. */
export type RpcNotification = (({
  "method": "events.batch";
  "params": EventsBatch;
  [key: string]: unknown;
})) & ({
  "jsonrpc": JsonRpcVersion;
  [key: string]: unknown;
});

/** RpcRequest; `protocol.md` §7.1. */
export type RpcRequest = (({
  "method": "runtime.hello";
  "params": HelloParams;
  [key: string]: unknown;
}) | ({
  "method": "runtime.heartbeat";
  "params": HeartbeatParams;
  [key: string]: unknown;
}) | ({
  "method": "host.report";
  "params": HostReportParams;
  [key: string]: unknown;
}) | ({
  "method": "host.get";
  "params": HostParams;
  [key: string]: unknown;
}) | ({
  "method": "host.list";
  "params": PageParams;
  [key: string]: unknown;
}) | ({
  "method": "driver.list";
  "params": HostParams;
  [key: string]: unknown;
}) | ({
  "method": "driver.capabilities";
  "params": DriverCapabilitiesParams;
  [key: string]: unknown;
}) | ({
  "method": "workspace.register";
  "params": CommandEnvelope2;
  [key: string]: unknown;
}) | ({
  "method": "workspace.get";
  "params": WorkspaceParams;
  [key: string]: unknown;
}) | ({
  "method": "workspace.list";
  "params": WorkspaceListParams;
  [key: string]: unknown;
}) | ({
  "method": "worktree.create";
  "params": CommandEnvelope3;
  [key: string]: unknown;
}) | ({
  "method": "worktree.remove";
  "params": CommandEnvelope4;
  [key: string]: unknown;
}) | ({
  "method": "instance.create";
  "params": CommandEnvelope5;
  [key: string]: unknown;
}) | ({
  "method": "instance.attach";
  "params": CommandEnvelope6;
  [key: string]: unknown;
}) | ({
  "method": "instance.open_terminal";
  "params": CommandEnvelope7;
  [key: string]: unknown;
}) | ({
  "method": "instance.resume";
  "params": CommandEnvelope8;
  [key: string]: unknown;
}) | ({
  "method": "instance.send";
  "params": CommandEnvelope9;
  [key: string]: unknown;
}) | ({
  "method": "instance.configure";
  "params": CommandEnvelope10;
  [key: string]: unknown;
}) | ({
  "method": "instance.fork";
  "params": CommandEnvelope11;
  [key: string]: unknown;
}) | ({
  "method": "instance.cancel";
  "params": CommandEnvelope12;
  [key: string]: unknown;
}) | ({
  "method": "instance.close";
  "params": CommandEnvelope13;
  [key: string]: unknown;
}) | ({
  "method": "instance.get";
  "params": InstanceParams;
  [key: string]: unknown;
}) | ({
  "method": "instance.list";
  "params": InstanceListParams;
  [key: string]: unknown;
}) | ({
  "method": "command.get";
  "params": CommandParams;
  [key: string]: unknown;
}) | ({
  "method": "command.list";
  "params": CommandListParams;
  [key: string]: unknown;
}) | ({
  "method": "run.get";
  "params": RunParams;
  [key: string]: unknown;
}) | ({
  "method": "run.list";
  "params": RunListParams;
  [key: string]: unknown;
}) | ({
  "method": "run.wait";
  "params": RunWaitParams;
  [key: string]: unknown;
}) | ({
  "method": "workflow.wait";
  "params": WorkflowWaitParams;
  [key: string]: unknown;
}) | ({
  "method": "interaction.list";
  "params": InteractionListParams;
  [key: string]: unknown;
}) | ({
  "method": "interaction.get";
  "params": InteractionParams;
  [key: string]: unknown;
}) | ({
  "method": "interaction.respond";
  "params": CommandEnvelope14;
  [key: string]: unknown;
}) | ({
  "method": "events.subscribe";
  "params": EventsSubscribeParams;
  [key: string]: unknown;
}) | ({
  "method": "events.read";
  "params": EventsReadParams;
  [key: string]: unknown;
}) | ({
  "method": "events.ack";
  "params": EventsAckParams;
  [key: string]: unknown;
}) | ({
  "method": "events.unsubscribe";
  "params": SubscriptionParams;
  [key: string]: unknown;
}) | ({
  "method": "reconcile.instance";
  "params": ReconcileInstanceParams;
  [key: string]: unknown;
}) | ({
  "method": "tty.attach";
  "params": TtyAttachParams;
  [key: string]: unknown;
}) | ({
  "method": "tty.detach";
  "params": TtyDetachParams;
  [key: string]: unknown;
}) | ({
  "method": "tty.write";
  "params": CommandEnvelope15;
  [key: string]: unknown;
}) | ({
  "method": "tty.resize";
  "params": TtyResizeParams;
  [key: string]: unknown;
}) | ({
  "method": "object.stat";
  "params": ObjectParams;
  [key: string]: unknown;
}) | ({
  "method": "object.read";
  "params": ObjectReadParams;
  [key: string]: unknown;
}) | ({
  "method": "object.prepare";
  "params": ObjectPrepareParams;
  [key: string]: unknown;
}) | ({
  "method": "object.write";
  "params": ObjectWriteParams;
  [key: string]: unknown;
}) | ({
  "method": "object.commit";
  "params": ObjectCommitParams;
  [key: string]: unknown;
})) & ({
  "id": (string);
  "jsonrpc": JsonRpcVersion;
  [key: string]: unknown;
});

/** A response contains exactly one of result/error; `protocol.md` §7.1. */
export type RpcResponse = (RpcSuccess | RpcFailure);

/** RpcSuccess; `protocol.md` §7.1. */
export type RpcSuccess = ({
  "id": (string);
  "jsonrpc": JsonRpcVersion;
  "result": unknown;
});

/** Run; `protocol.md` §2.4. */
export type Run = ({
  "capabilitySnapshotId": Id;
  "cause": RunCause;
  "completionScope": CompletionScope;
  "createdAt": Timestamp;
  "endedAt": Knowledge9;
  "hostId": HostId;
  "id": RunId;
  "inputDigest": Knowledge;
  "inputRef": (Id | (null));
  "instanceId": InstanceId;
  "nativeTurns": ((NativeTurn)[]);
  "outstandingWork": Knowledge22;
  "parentRunId": (RunId | (null));
  "parentage": Parentage;
  "processGeneration": U64;
  "providerSelection": ProviderSelection;
  "result": Knowledge21;
  "revision": U64;
  "rootRunId": RunId;
  "runGeneration": U64;
  "startedAt": Knowledge9;
  "state": RunState;
  "stateConfidence": StateConfidence;
  "terminalEvidence": Knowledge20;
  "updatedAt": Timestamp;
  [key: string]: unknown;
});

/** RunCause; `protocol.md` §2.4. */
export type RunCause = ({
  "commandId": (CommandId | (null));
  "sourceEventId": (EventId | (null));
  "type": RunCauseType;
  [key: string]: unknown;
});

/** RunCauseType wire values; `protocol.md` §2.4. */
export type RunCauseType = ("command" | "native-continuation" | "external-input" | "import");

export type RunId = (string);

/** RunListParams; `protocol.md` §7.2. */
export type RunListParams = ({
  "cursor"?: (string | null);
  "instanceId": InstanceId;
  "limit": (number);
  [key: string]: unknown;
});

/** RunParams; `protocol.md` §7.2. */
export type RunParams = ({
  "runId": RunId;
  [key: string]: unknown;
});

/** RunResult; `protocol.md` §2.4. */
export type RunResult = ({
  "artifactIds": ((Id)[]);
  "messageIds": ((Id)[]);
  "outputRef": (Id | (null));
  [key: string]: unknown;
});

/** RunState wire values; `protocol.md` §2.4. */
export type RunState = ("queued" | "running" | "waiting-interaction" | "draining" | "succeeded" | "failed" | "cancelled" | "unknown" | "reconciling");

/** RunWaitCondition wire values; `protocol.md` §7.2. */
export type RunWaitCondition = ("terminal" | "interaction" | "observed-update");

/** RunWaitParams; `protocol.md` §7.2. */
export type RunWaitParams = ({
  "afterSeq"?: (U64 | (null));
  "condition": RunWaitCondition;
  "runId": RunId;
  "timeoutMs": (number);
  [key: string]: unknown;
});

/** RunWaitResult; `protocol.md` §7.2. */
export type RunWaitResult = ({
  "asOfSeq": U64;
  "pendingInteractionIds": ((InteractionId)[]);
  "reason": WaitReason;
  "run": Run;
  [key: string]: unknown;
});

/** One capability this live session actually reached, with the tier that proves it; `protocol.md` §1.3 (D-028 §4.3).  A runtime entry outranks the static `DriverKind` matrix for the same name. It carries its own `state`, so a session may report a capability as `unknown` just as truthfully as `supported`. */
export type RuntimeCapability = ({
  "name": CapabilityName;
  "provision": CapabilityProvision;
  "reasonCode": (string);
  "state": CapabilityState;
  "tier": SignalTier;
  [key: string]: unknown;
});

/** RuntimeCursor; `protocol.md` §5.1. */
export type RuntimeCursor = ({
  "ledgerRevision": U64;
  [key: string]: unknown;
});

/** RuntimeError; `protocol.md` §9.1. */
export type RuntimeError = ({
  "code": ErrorCode;
  "details": ErrorDetails;
  "execution": ExecutionState;
  "message": (string);
  "retry": RetryAction;
  "rpcCode": (number);
  [key: string]: unknown;
});

/** SandboxExecution; `protocol.md` §4.1. */
export type SandboxExecution = ({
  "sandbox": SandboxMode;
});

/** SandboxMode wire values; `protocol.md` §4.1. */
export type SandboxMode = ("read-only" | "workspace-write" | "danger-full-access");

export type SchemaVersion = 1;

/** SelectionReason wire values; `protocol.md` §4.1. */
export type SelectionReason = ("pinned" | "weighted-healthy" | "explicit-recovery");

/** SendInput; `protocol.md` §7.2. */
export type SendInput = (PromptInput & ({
  "type": "prompt";
  [key: string]: unknown;
}) | SteerInput & ({
  "type": "steer";
  [key: string]: unknown;
}));

/** Generic low/normal/high sensitivity knob. */
export type Sensitivity = ("low" | "normal" | "high");

/** SettingsFormat wire values; `protocol.md` §4.1. */
export type SettingsFormat = ("claude-json" | "codex-toml" | "grok-toml" | "agy-json" | "none");

/** SettingsOverlay; `protocol.md` §4.1. */
export type SettingsOverlay = ({
  "format": SettingsFormat;
  "objectRef": (Id | (null));
  "revision": U64;
  [key: string]: unknown;
});

/** Settlement; `protocol.md` §2.5. */
export type Settlement = ({
  "error": (RuntimeError | (null));
  "outcome": SettlementOutcome;
  "resultRef": (Id | (null));
  [key: string]: unknown;
});

/** SettlementOutcome wire values; `protocol.md` §2.5. */
export type SettlementOutcome = ("completed" | "rejected" | "cancelled" | "expired");

/** Severity wire values; `protocol.md` §5.5. */
export type Severity = ("info" | "warning" | "error");

/** SignalTier wire values; `protocol.md` §1.3. */
export type SignalTier = ("hook" | "file" | "osc" | "screen" | "none");

/** Snapshot; `protocol.md` §7.3. */
export type Snapshot = (InstanceSnapshot & ({
  "scope": "instance";
  [key: string]: unknown;
}) | RegistrySnapshot & ({
  "scope": "registry";
  [key: string]: unknown;
}));

/** SnapshotMode wire values; `protocol.md` §7.3. */
export type SnapshotMode = ("required" | "if-needed" | "none");

/** SourceChannel wire values; `protocol.md` §5.1. */
export type SourceChannel = ("stdout" | "stderr" | "transcript" | "workflow-journal" | "hook" | "rpc" | "pty" | "herdr" | "runtime" | "file" | "osc" | "screen");

/** SourceCursor; `protocol.md` §5.1. */
export type SourceCursor = (StreamCursor & ({
  "type": "stream";
  [key: string]: unknown;
}) | FileCursor & ({
  "type": "file";
  [key: string]: unknown;
}) | HookCursor & ({
  "type": "hook";
  [key: string]: unknown;
}) | TtyCursor & ({
  "type": "tty";
  [key: string]: unknown;
}) | RuntimeCursor & ({
  "type": "runtime";
  [key: string]: unknown;
}));

/** SourceDelivery wire values; `protocol.md` §5.1. */
export type SourceDelivery = ("live" | "replay" | "unknown");

/** Spend-control panel numbers (billing-site data, never inferred); §4.2. */
export type SpendControl = ({
  "limit"?: (string | null);
  "remainingPercent"?: (number | null);
  "used"?: (string | null);
  [key: string]: unknown;
});

/** StateConfidence wire values; `protocol.md` §2.4. */
export type StateConfidence = ("confirmed" | "unknown");

/** SteerInput; `protocol.md` §3.1. */
export type SteerInput = ({
  "blocks": ((ContentBlock)[]);
  "expectedNativeTurnId": (string);
  "nativeClientMessageId": (string);
  [key: string]: unknown;
});

/** StreamCursor; `protocol.md` §5.1. */
export type StreamCursor = ({
  "connectionEpoch": Id;
  "frame": U64;
  [key: string]: unknown;
});

export type StreamUuid = (string);

/** SubscriptionParams; `protocol.md` §7.3. */
export type SubscriptionParams = ({
  "subscriptionId": Id;
  [key: string]: unknown;
});

/** Account-level concurrency ceiling; §4.4 step 2 — the primitive that was entirely missing while only host `maxInstances` existed. */
export type SupplyConcurrency = ({
  "max"?: (number | null);
  [key: string]: unknown;
});

/** Subscription/credits presence; §4.2. */
export type SupplyCredits = ({
  "hasCredits"?: (boolean | null);
  "unlimited"?: (boolean);
  [key: string]: unknown;
});

/** Declared supply plus observed state for one provider profile; §4.2.  Every field is optional/defaulted so a user can declare as little as "workhorse: X, scarce: Y" and grow from there. Observed runtime fields (`state`, `cooldownUntil`, `lastError`, window cooldowns) are written by the Hub feedback loop, never by the user. */
export type SupplyProfile = ({
  "concurrency": SupplyConcurrency;
  "cooldownUntil"?: (number | null);
  "credits"?: (SupplyCredits | (null));
  "dailyUsd"?: (number | null);
  "lastError"?: (string | null);
  "ordinaryUsageAllowed"?: (boolean | null);
  "priority"?: (number);
  "reserve": SupplyReserve;
  "resetWindowMins"?: (number | null);
  "spendControl"?: (SpendControl | (null));
  "state": SupplyState;
  "weeklyUsd"?: (number | null);
  "windows"?: ((RateLimitWindow)[]);
  [key: string]: unknown;
});

/** Who may spend a supply; §4.2 (`reserve`). */
export type SupplyReserve = ("none" | "coordinator-only");

/** Account-level supply state; §4.2. */
export type SupplyState = ("available" | "degraded" | "cooling" | "exhausted" | "unknown");

/** One row of the task ledger; design §2.2/§8.1 row 4. */
export type Task = ({
  "blockedReason"?: (string | null);
  "budget": TaskBudget;
  "class": TaskClass;
  "createdAt": Timestamp;
  "deps"?: ((TaskDep)[]);
  "id": TaskId;
  "landedSha"?: (string | null);
  "mandate": Mandate;
  "owns"?: (((string))[]);
  "parentTaskId"?: (TaskId | (null));
  "placement"?: (TaskPlacementRef | (null));
  "projectId": ProjectId;
  "revision": U64;
  "state": TaskState;
  "title": (string);
  "updatedAt": Timestamp;
  [key: string]: unknown;
});

/** Estimated budget envelope; design §4.3. Amounts are estimates (§4.5). */
export type TaskBudget = ({
  "maxTurns"?: (number | null);
  "maxUsd"?: (number | null);
  "maxWallMins"?: (number | null);
  [key: string]: unknown;
});

/** TaskClass wire values; `protocol.md` §4.3. */
export type TaskClass = ("research" | "implement" | "review" | "test" | "merge-gate" | "triage" | "docs");

/** One dependency edge. Edges unlock only when the referenced task carries a landed sha (invariant I1, design §7 #8). */
export type TaskDep = ({
  "note"?: (string | null);
  "taskId": TaskId;
  [key: string]: unknown;
});

export type TaskId = (string);

/** One parsed `<task-notification>` body. */
export type TaskNotification = ({
  "result": (string | null);
  "status": (string);
  "summary": (string | null);
  "task_id": (string);
  "tool_use_id": (string | null);
  [key: string]: unknown;
});

/** Explicit pin that disables automatic supply choice; §4.3. */
export type TaskPin = ({
  "harness"?: (string | null);
  "model"?: (string | null);
  "supplyId"?: (string | null);
  [key: string]: unknown;
});

/** Where the task's worker is (or was) placed; updated from placement rows. */
export type TaskPlacementRef = ({
  "branch"?: (string | null);
  "hostId"?: (HostId | (null));
  "instanceId"?: (InstanceId | (null));
  "model"?: (string | null);
  "placementId"?: (Id | (null));
  [key: string]: unknown;
});

/** What a coordinator submits at dispatch/instance-create; §4.3.  Carried additively on the instance create body as `taskSpec`. Only `class`/sensitivities are LLM assignments; the rest is planner bookkeeping. */
export type TaskSpec = ({
  "allowDowngrade"?: (boolean);
  "budget"?: (TaskBudget | (null));
  "class": TaskClass;
  "contextNeed": ContextNeed;
  "costSensitivity": Sensitivity;
  "effort"?: (string | null);
  "latencySensitivity": Sensitivity;
  "minClass"?: (ModelClass | (null));
  "parent"?: (TaskId | (null));
  "pin"?: (TaskPin | (null));
  "projectId"?: (ProjectId | (null));
  "requires"?: (((string))[]);
  "taskId"?: (TaskId | (null));
  [key: string]: unknown;
});

/** TaskState wire values; `protocol.md` §2.2/5.3. */
export type TaskState = ("pending" | "placed" | "running" | "stalled" | "done" | "failed" | "parked" | "deferred");

/** TerminalEvidence; `protocol.md` §2.4. */
export type TerminalEvidence = ({
  "eventIds": ((EventId)[]);
  "nativeOutcome": (string);
  "ruleId": (string);
  [key: string]: unknown;
});

/** TextBlock; `protocol.md` §5.2. */
export type TextBlock = ({
  "text": (string);
  [key: string]: unknown;
});

/** ThoughtPayload; `protocol.md` §5.2. */
export type ThoughtPayload = ({
  "baseRevision": (U64 | (null));
  "nodeId": Id;
  "operation": MutationOperation;
  "partIndex": (number);
  "representation": ThoughtRepresentation;
  "revision": U64;
  "status": ContentStatus;
  "text": (string | null);
  "thoughtId": Id;
  [key: string]: unknown;
});

/** ThoughtRepresentation wire values; `protocol.md` §5.2. */
export type ThoughtRepresentation = ("summary" | "text" | "redacted");

export type Timestamp = (string);

/** ToolCallPayload; `protocol.md` §5.2. */
export type ToolCallPayload = ({
  "baseRevision": (U64 | (null));
  "category": ToolCategory;
  "displayTitle": Knowledge2;
  "executor": Knowledge12;
  "input": Knowledge11;
  "inputTextDelta": (string | null);
  "nodeId": Id;
  "operation": MutationOperation;
  "parentToolCallId": (Id | (null));
  "revision": U64;
  "state": ToolCallState;
  "toolCallId": Id;
  "toolName": Knowledge2;
  [key: string]: unknown;
});

/** ToolCallState wire values; `protocol.md` §5.2. */
export type ToolCallState = ("proposed" | "running" | "unknown");

/** ToolCategory wire values; `protocol.md` §5.2. */
export type ToolCategory = ("shell" | "file-read" | "file-write" | "search" | "mcp" | "workflow" | "agent" | "other");

/** ToolOutcome wire values; `protocol.md` §5.2. */
export type ToolOutcome = ("succeeded" | "failed" | "denied" | "cancelled" | "unknown");

/** ToolResultPayload; `protocol.md` §5.2. */
export type ToolResultPayload = ({
  "baseRevision": (U64 | (null));
  "blocks": ((ContentBlock)[]);
  "changes": ((FileChange)[]);
  "exitCode": Knowledge13;
  "nodeId": Id;
  "operation": MutationOperation;
  "outcome": ToolOutcome;
  "revision": U64;
  "stage": ResultStage;
  "structuredResult": Knowledge11;
  "toolCallId": Id;
  [key: string]: unknown;
});

/** TranscriptRef; `protocol.md` §1.3. */
export type TranscriptRef = ({
  "objectId": Id;
  "sourcePath": (string);
  [key: string]: unknown;
});

/** TransportLimits; `protocol.md` §7.4. */
export type TransportLimits = ({
  "heartbeatIntervalMs": (number);
  "leaseTtlMs": (number);
  "maxBinaryChunkBytes": (number);
  "maxEventsPerBatch": (number);
  "maxInFlightRpc": (number);
  "maxJsonFrameBytes": (number);
  "maxSubscriptionBufferEvents": (number);
  "maxTtyInputBytes": (number);
  "maxWaitMs": (number);
  [key: string]: unknown;
});

/** TtyAttachParams; `protocol.md` §7.4. */
export type TtyAttachParams = ({
  "afterOffset": (U64 | (null));
  "instanceId": InstanceId;
  "mode": TtyMode;
  "previousStreamId": (Id | (null));
  "processGeneration": U64;
});

/** TtyAttachResult; `protocol.md` §7.4. */
export type TtyAttachResult = ({
  "altScreen"?: (boolean | null);
  "availableFrom": U64;
  "nextOffset": U64;
  "representation": TtyRepresentation;
  "screenSnapshotRef": (Id | (null));
  "snapshotAtOffset": Knowledge3;
  "snapshotBase64"?: (string | null);
  "streamEpoch": Id;
  "streamId": Id;
  "writerLease": (TtyWriterLease | (null));
  [key: string]: unknown;
});

/** TtyCursor; `protocol.md` §5.1. */
export type TtyCursor = ({
  "length": U64;
  "offset": U64;
  "streamId": Id;
  [key: string]: unknown;
});

/** TtyDetachParams; `protocol.md` §7.4. */
export type TtyDetachParams = ({
  "instanceId": InstanceId;
  "streamId": Id;
  "writerLeaseId"?: (Id | (null));
  [key: string]: unknown;
});

/** TtyInput; `protocol.md` §5.5. */
export type TtyInput = ({
  "actor": ActorRef;
  "byteLength": U64;
  "dataRef": (RawRef | (null));
  "delivery": TtyInputDelivery;
  "inputId": Id;
  "streamEpoch": Id;
  "streamId": Id;
  [key: string]: unknown;
});

/** TtyInputDelivery wire values; `protocol.md` §5.5. */
export type TtyInputDelivery = ("written" | "unknown");

/** TtyMode wire values; `protocol.md` §7.4. */
export type TtyMode = ("read" | "write");

/** TtyOutput; `protocol.md` §5.5. */
export type TtyOutput = ({
  "byteLength": U64;
  "dataRef": RawRef;
  "nativeFrame": Knowledge26;
  "offset": U64;
  "representation": TtyRepresentation;
  "streamEpoch": Id;
  "streamId": Id;
  [key: string]: unknown;
});

/** TtyRepresentation wire values; `protocol.md` §5.5. */
export type TtyRepresentation = ("pty-bytes" | "rendered-ansi");

/** TtyResize; `protocol.md` §5.5. */
export type TtyResize = ({
  "cols": (number);
  "resizeRevision": U64;
  "rows": (number);
  "streamEpoch": Id;
  "streamId": Id;
  [key: string]: unknown;
});

/** TtyResizeParams; `protocol.md` §7.4. */
export type TtyResizeParams = ({
  "cols": (number);
  "instanceId": InstanceId;
  "resizeRevision": U64;
  "rows": (number);
  "streamId": Id;
  "writerLeaseId": Id;
  [key: string]: unknown;
});

/** TtyResizeResult; `protocol.md` §7.4. */
export type TtyResizeResult = ({
  "cols": (number);
  "resizeRevision": U64;
  "rows": (number);
  [key: string]: unknown;
});

/** TtyWriteParams; `protocol.md` §7.4. */
export type TtyWriteParams = ({
  "dataBase64": (string);
  "inputSeq": U64;
  "instanceId": InstanceId;
  "processGeneration": U64;
  "streamEpoch": Id;
  "streamId": Id;
  "writerLeaseId": Id;
  [key: string]: unknown;
});

/** TtyWriterLease; `protocol.md` §7.4. */
export type TtyWriterLease = ({
  "expiresAt": Timestamp;
  "inputNextSeq": U64;
  "leaseId": Id;
  [key: string]: unknown;
});

/** Claude renderer requested at launch (D-028 §9.2).  This is intent only; the terminal snapshot's `altScreen` reports the observed screen state after Claude applies platform and accessibility rules. */
export type TuiMode = ("fullscreen" | "default");

export type U64 = (string);

/** UrlLocator; `protocol.md` §5.5. */
export type UrlLocator = ({
  "url": (string);
  [key: string]: unknown;
});

/** UsageMode wire values; `protocol.md` §5.5. */
export type UsageMode = ("snapshot" | "delta");

/** UsagePayload; `protocol.md` §5.5. */
export type UsagePayload = ({
  "accounting": Accounting;
  "cacheReadTokens": Knowledge3;
  "cacheWriteTokens": Knowledge3;
  "cost": Knowledge25;
  "inputAccounting": InputAccounting;
  "inputTokens": Knowledge3;
  "metricRevision": U64;
  "mode": UsageMode;
  "nativeFieldsRef": (Id | (null));
  "outputTokens": Knowledge3;
  "reasoningTokens": Knowledge3;
  "scope": UsageScope;
  "scopeId": (string);
  "totalTokens": Knowledge3;
  "usageId": Id;
  [key: string]: unknown;
});

/** UsageScope wire values; `protocol.md` §5.5. */
export type UsageScope = ("message" | "turn" | "session" | "workflow-member");

/** WaitReason wire values; `protocol.md` §7.2. */
export type WaitReason = ("condition-met" | "timeout" | "unknown");

/** Where a window's numbers came from; §4.1.  Observed frames update `observed` fields but never erase a `declared` limit; `inferred` is filled by Hub-side usage aggregation (§4.5). */
export type WindowSource = ("declared" | "observed" | "inferred");

/** `worker.provision` params: create the product-assigned worktree and the per-worker cargo target directory on the Node.  The Node owns the filesystem layout: the request names the worker and the branch, but never absolute paths (security-review-2 M4, same rule as `worktree.create`). The Node creates the worktree under its managed `<repo>/../remuda-wt/<name>` root and the target dir under `<repo>/../remuda-target/<name>`. */
export type WorkerProvisionParams = ({
  "branch": (string);
  "name": (string);
  "startPoint"?: (string | null);
  "workspaceId"?: (WorkspaceId | (null));
  [key: string]: unknown;
});

/** Result of `worker.provision`. */
export type WorkerProvisionResult = ({
  "branch": (string);
  "name": (string);
  "startPoint": (string);
  "targetDir": (string);
  "worktreePath": (string);
  [key: string]: unknown;
});

/** `worker.remove` params: reclaim one worker's filesystem resources.  Paths are never taken from the wire: the Node recomputes both locations from the managed roots and the `name`, then containment-checks before deleting. */
export type WorkerRemoveParams = ({
  "instanceId"?: (InstanceId | (null));
  "name": (string);
  "workspaceId"?: (WorkspaceId | (null));
  [key: string]: unknown;
});

/** Result of `worker.remove`. */
export type WorkerRemoveResult = ({
  "name": (string);
  "reclaimedBytes": U64;
  "targetRemoved": (boolean);
  "worktreeRemoved": (boolean);
  [key: string]: unknown;
});

/** One row of the per-project worker roster. */
export type WorkerRoster = ({
  "branch": (string);
  "briefObjectId"?: (string | null);
  "createdAt": Timestamp;
  "harness": (string);
  "hostId": HostId;
  "id": WorkerRosterId;
  "instanceId"?: (InstanceId | (null));
  "lastNudgeAt"?: (Timestamp | (null));
  "model"?: (string | null);
  "name": (string);
  "portBlock"?: (string | null);
  "projectId": ProjectId;
  "providerProfileId"?: (string | null);
  "reclaimedBytes"?: (U64 | (null));
  "replaceCount"?: (U64 | (null));
  "resumedFrom"?: (InstanceId | (null));
  "revision": U64;
  "state": WorkerState;
  "supplyDecision"?: unknown;
  "targetDir"?: (string | null);
  "taskId"?: (TaskId | (null));
  "updatedAt": Timestamp;
  "watch"?: (WorkerWatch | (null));
  "workspaceId": WorkspaceId;
  "worktreePath": (string);
  [key: string]: unknown;
});

export type WorkerRosterId = (string);

/** Lifecycle of a dispatched worker; design §1.1 goal 6.  Wire shape is adjacently tagged: `{"state":"done","sha":"…"}`. */
export type WorkerState = (({
  "state": "dispatched";
  [key: string]: unknown;
}) | ({
  "state": "working";
  [key: string]: unknown;
}) | ({
  "sha": (string);
  "state": "done";
  [key: string]: unknown;
}) | ({
  "reason": (string);
  "state": "blocked";
  [key: string]: unknown;
}) | ({
  "state": "retired";
  [key: string]: unknown;
}));

/** One persisted `remuda watch` observation on a roster row.  Besides the current status it carries the echo-suppression baselines (`lastDoneSha` / `lastBlocked` — a resumed session re-shows its old DONE) and the activity bookkeeping the stall detector needs across polls. */
export type WorkerWatch = (({
  "status": "working";
  [key: string]: unknown;
}) | ({
  "sha": (string);
  "status": "done";
  [key: string]: unknown;
}) | ({
  "reason": (string);
  "status": "blocked";
  [key: string]: unknown;
}) | ({
  "status": "idle-api-error";
  [key: string]: unknown;
}) | ({
  "status": "stalled";
  [key: string]: unknown;
}) | ({
  "status": "gone";
  [key: string]: unknown;
}) | ({
  "reason": (string);
  "status": "failed";
  [key: string]: unknown;
})) & ({
  "detail"?: (string | null);
  "lastActivityAt"?: (Timestamp | (null));
  "lastBlocked"?: (string | null);
  "lastDoneSha"?: (string | null);
  "lastScreenDigest"?: (string | null);
  "observedAt": Timestamp;
  [key: string]: unknown;
});

/** The point-in-time status `remuda watch` derives from a worker's screen.  Adjacently tagged on `status`, so the wire shape is `{"status":"done","sha":"…"}` / `{"status":"blocked","reason":"…"}`. */
export type WorkerWatchStatus = (({
  "status": "working";
  [key: string]: unknown;
}) | ({
  "sha": (string);
  "status": "done";
  [key: string]: unknown;
}) | ({
  "reason": (string);
  "status": "blocked";
  [key: string]: unknown;
}) | ({
  "status": "idle-api-error";
  [key: string]: unknown;
}) | ({
  "status": "stalled";
  [key: string]: unknown;
}) | ({
  "status": "gone";
  [key: string]: unknown;
}) | ({
  "reason": (string);
  "status": "failed";
  [key: string]: unknown;
}));

/** WorkflowEngine wire values; `protocol.md` §5.3. */
export type WorkflowEngine = ("claude-workflow");

/** The card's live line under the header; additive r-ux-w, §5.3. */
export type WorkflowLive = ({
  "agentLabel": Knowledge2;
  "phaseTitle": Knowledge2;
  "summary": Knowledge2;
  [key: string]: unknown;
});

/** WorkflowMemberPayload; `protocol.md` §5.3. */
export type WorkflowMemberPayload = ({
  "attempt": Knowledge3;
  "calls"?: (U64 | (null));
  "durationMs"?: (U64 | (null));
  "endedAt"?: (Timestamp | (null));
  "label": Knowledge2;
  "latestTool"?: (Knowledge2 | (null));
  "memberId": Id;
  "modelRequested": Knowledge2;
  "modelResolved": Knowledge2;
  "nativeAgentId": Knowledge2;
  "nativeKey": Knowledge2;
  "phaseId": (Id | (null));
  "resultRef": (Id | (null));
  "revision": U64;
  "startedAt"?: (Timestamp | (null));
  "state": WorkflowState;
  "tokens"?: (U64 | (null));
  "workflowId": Id;
  [key: string]: unknown;
});

/** WorkflowPhasePayload; `protocol.md` §5.3. */
export type WorkflowPhasePayload = ({
  "label": Knowledge2;
  "nativePhaseId": Knowledge2;
  "parentPhaseId": (Id | (null));
  "phaseId": Id;
  "revision": U64;
  "state": WorkflowState;
  "workflowId": Id;
  [key: string]: unknown;
});

/** WorkflowRunPayload; `protocol.md` §5.3. */
export type WorkflowRunPayload = ({
  "description"?: (Knowledge2 | (null));
  "engine": WorkflowEngine;
  "live"?: (WorkflowLive | (null));
  "name"?: (Knowledge2 | (null));
  "nativeRunId": Knowledge2;
  "nativeTaskId": Knowledge2;
  "note"?: (string | null);
  "resultRef": (Id | (null));
  "revision": U64;
  "state": WorkflowState;
  "title": Knowledge2;
  "toolCallId": (Id | (null));
  "totals"?: (WorkflowTotals | (null));
  "workflowId": Id;
  [key: string]: unknown;
});

/** WorkflowState wire values; `protocol.md` §5.3. */
export type WorkflowState = ("queued" | "running" | "completed" | "failed" | "cancelled" | "unknown");

/** Aggregate counters shown on a Workflow card; additive r-ux-w, §5.3.  All values are a point-in-time snapshot: a running run's `tokens` / `elapsed_ms` keep moving until the terminal revision. */
export type WorkflowTotals = ({
  "agentsDone": U64;
  "agentsFailed": U64;
  "agentsKilled": U64;
  "agentsRunning": U64;
  "agentsTotal": U64;
  "calls": U64;
  "elapsedMs": U64;
  "tokens": U64;
  "totalKnown": (boolean);
  [key: string]: unknown;
});

/** WorkflowWaitParams; `protocol.md` §7.2. */
export type WorkflowWaitParams = ({
  "afterSeq"?: (U64 | (null));
  "instanceId": InstanceId;
  "timeoutMs": (number);
  "workflowId": Id;
  [key: string]: unknown;
});

/** WorkflowWaitResult; `protocol.md` §7.2. */
export type WorkflowWaitResult = ({
  "asOfSeq": U64;
  "reason": WaitReason;
  "workflow": WorkflowRunPayload;
  [key: string]: unknown;
});

/** Workspace; `protocol.md` §2.2. */
export type Workspace = ({
  "accessPolicyRevision": U64;
  "canonicalRoot": Knowledge2;
  "createdAt": Timestamp;
  "durableSeq": U64;
  "hostId": HostId;
  "id": WorkspaceId;
  "journalId": Id;
  "label": (string);
  "repository": Knowledge16;
  "revision": U64;
  "rootPath": (string);
  "state": WorkspaceState;
  "updatedAt": Timestamp;
  "worktree": (WorktreeRecord | (null));
  "writePolicy": WritePolicy;
  "writerLeases": ((WriterLease)[]);
  [key: string]: unknown;
});

/** WorkspaceFileLocator; `protocol.md` §5.5. */
export type WorkspaceFileLocator = ({
  "digest": Knowledge;
  "relativePath": (string);
  "revision": U64;
  "workspaceId": WorkspaceId;
  [key: string]: unknown;
});

export type WorkspaceId = (string);

/** WorkspaceListParams; `protocol.md` §7.2. */
export type WorkspaceListParams = ({
  "cursor"?: (string | null);
  "hostId": HostId;
  "limit": (number);
  [key: string]: unknown;
});

/** WorkspaceParams; `protocol.md` §7.2. */
export type WorkspaceParams = ({
  "workspaceId": WorkspaceId;
  [key: string]: unknown;
});

/** WorkspaceRegisterParams; `protocol.md` §7.2. */
export type WorkspaceRegisterParams = ({
  "label": (string);
  "rootPath": (string);
  "workspaceId": WorkspaceId;
  "writePolicy": WritePolicy;
  [key: string]: unknown;
});

/** WorkspaceState wire values; `protocol.md` §2.2. */
export type WorkspaceState = ("registering" | "ready" | "unavailable" | "archived" | "failed");

/** WorktreeCreateParams; `protocol.md` §7.2. */
export type WorktreeCreateParams = ({
  "baseOid": (string);
  "branch": (string);
  "parentWorkspaceId": WorkspaceId;
  "path"?: (string | null);
  "worktreeId": WorktreeId;
  [key: string]: unknown;
});

export type WorktreeId = (string);

/** WorktreeOwner wire values; `protocol.md` §2.2. */
export type WorktreeOwner = ("runtime" | "native" | "external");

/** WorktreeRecord; `protocol.md` §2.2. */
export type WorktreeRecord = ({
  "baseOid": Knowledge2;
  "branch": Knowledge2;
  "createdByCommandId": (CommandId | (null));
  "dirty": Knowledge17;
  "headOid": Knowledge2;
  "hostId": HostId;
  "id": WorktreeId;
  "managedBy": WorktreeOwner;
  "parentWorkspaceId": WorkspaceId;
  "path": (string);
  "repositoryId": Id;
  "state": WorktreeState;
  [key: string]: unknown;
});

/** WorktreeRemoveParams; `protocol.md` §7.2. */
export type WorktreeRemoveParams = ({
  "expectedDirty": BoolLiteral_false;
  "expectedHeadOid": (string);
  "worktreeId": WorktreeId;
  [key: string]: unknown;
});

/** WorktreeSpec; `protocol.md` §4.1. */
export type WorktreeSpec = (ExistingWorktree & ({
  "mode": "existing";
  [key: string]: unknown;
}) | CreateWorktree & ({
  "mode": "create";
  [key: string]: unknown;
}));

/** WorktreeState wire values; `protocol.md` §2.2. */
export type WorktreeState = ("creating" | "ready" | "unavailable" | "removed" | "failed");

/** WritePolicy wire values; `protocol.md` §2.2. */
export type WritePolicy = ("exclusive" | "isolated-worktree" | "shared-explicit");

/** WriterLease; `protocol.md` §2.2. */
export type WriterLease = ({
  "fence": U64;
  "instanceId": InstanceId;
  [key: string]: unknown;
});
