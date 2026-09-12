import type { DriverKind, NativeRequestKey } from "./nativeRef";
import type { Interaction } from "./interaction";
import type { Digest, Id, Json, Knowledge, Timestamp, U64 } from "./wire";

export type ObservationKind =
  | "message"
  | "thought"
  | "tool_call"
  | "tool_result"
  | "interaction.requested"
  | "interaction.answered"
  | "interaction.expired"
  | "workflow.run"
  | "workflow.phase"
  | "workflow.member"
  | "lifecycle"
  | "usage"
  | "artifact"
  | "raw_tty"
  | "opaque";

export type Completeness = "structured" | "partial" | "screen-derived" | "opaque";

export type SourceCursor =
  | { type: "stream"; connectionEpoch: Id; frame: U64 }
  | { type: "file"; fileIdentity: Id; fileGeneration: U64; offset: U64; length: U64; digest: Digest }
  | { type: "hook"; invocationId: Id; frame: U64 }
  | { type: "tty"; streamId: Id; offset: U64; length: U64 }
  | { type: "runtime"; ledgerRevision: U64 };

export type ObservationSource = {
  driverKind: DriverKind;
  driverVersion: string;
  adapterVersion: string;
  channel: "stdout" | "stderr" | "transcript" | "workflow-journal" | "hook" | "rpc" | "pty" | "herdr" | "runtime";
  delivery: "live" | "replay" | "unknown";
  nativeSessionId: Knowledge<string>;
  nativeTurnId: Knowledge<string>;
  nativeAgentId: Knowledge<string>;
  nativeItemId: Knowledge<string>;
  nativeEventId: Knowledge<string>;
  nativeRequestId: NativeRequestKey;
  sourceCursor: SourceCursor;
};

export type RawRef = {
  objectId: Id;
  offset: U64;
  length: U64;
  digest: Digest;
  mediaType: string;
  redaction: "none" | "derived-redacted" | "unavailable";
};

export type ContentBlock =
  | { type: "text"; text: string }
  | { type: "image" | "audio" | "file"; objectId: Id; mediaType: string; name: string | null }
  | { type: "resource"; uri: string; mediaType: Knowledge<string>; objectId: Id | null }
  | { type: "opaque"; rawRef: RawRef; nativeType: string };

export type NodeMutation = {
  nodeId: Id;
  revision: U64;
  operation: "open" | "append" | "replace" | "close";
  baseRevision: U64 | null;
};

export type MessagePayload = NodeMutation & {
  messageId: Id;
  role: "user" | "assistant" | "system";
  phase: "input" | "commentary" | "final" | "unknown";
  blocks: ContentBlock[];
  targetBlock: number | null;
  parentToolCallId: Id | null;
  nativeOrigin: Knowledge<string>;
  status: "streaming" | "complete" | "interrupted" | "unknown";
};

export type ThoughtPayload = NodeMutation & {
  thoughtId: Id;
  representation: "summary" | "text" | "redacted";
  text: string | null;
  partIndex: number;
  status: "streaming" | "complete" | "interrupted" | "unknown";
};

export type ToolCallPayload = NodeMutation & {
  toolCallId: Id;
  parentToolCallId: Id | null;
  toolName: Knowledge<string>;
  displayTitle: Knowledge<string>;
  category: "shell" | "file-read" | "file-write" | "search" | "mcp" | "workflow" | "agent" | "other";
  input: Knowledge<Json>;
  inputTextDelta: string | null;
  state: "proposed" | "running" | "unknown";
  executor: Knowledge<{ hostId: Id; workspaceId: Id | null; nativeAgentId: string | null }>;
};

export type ToolResultPayload = NodeMutation & {
  toolCallId: Id;
  stage: "partial" | "final";
  outcome: "succeeded" | "failed" | "denied" | "cancelled" | "unknown";
  blocks: ContentBlock[];
  structuredResult: Knowledge<Json>;
  exitCode: Knowledge<number>;
  changes: { path: string; diff: string; application: "proposed" | "applied" | "unknown" }[];
};

export type WorkflowRunPayload = {
  workflowId: Id;
  engine: "claude-workflow";
  nativeRunId: Knowledge<string>;
  nativeTaskId: Knowledge<string>;
  toolCallId: Id | null;
  state: "queued" | "running" | "completed" | "failed" | "cancelled" | "unknown";
  revision: U64;
  title: Knowledge<string>;
  resultRef: Id | null;
};

export type WorkflowPhasePayload = {
  workflowId: Id;
  phaseId: Id;
  nativePhaseId: Knowledge<string>;
  label: Knowledge<string>;
  state: WorkflowRunPayload["state"];
  revision: U64;
  parentPhaseId: Id | null;
};

export type WorkflowMemberPayload = {
  workflowId: Id;
  memberId: Id;
  nativeAgentId: Knowledge<string>;
  nativeKey: Knowledge<string>;
  attempt: Knowledge<U64>;
  phaseId: Id | null;
  label: Knowledge<string>;
  state: WorkflowRunPayload["state"];
  modelRequested: Knowledge<string>;
  modelResolved: Knowledge<string>;
  resultRef: Id | null;
  revision: U64;
};

export type UsagePayload = {
  usageId: Id;
  scope: "message" | "turn" | "session" | "workflow-member";
  scopeId: string;
  mode: "snapshot" | "delta";
  metricRevision: U64;
  inputTokens: Knowledge<U64>;
  inputAccounting: "total-including-cache" | "uncached" | "provider-specific" | "unknown";
  outputTokens: Knowledge<U64>;
  reasoningTokens: Knowledge<U64>;
  cacheReadTokens: Knowledge<U64>;
  cacheWriteTokens: Knowledge<U64>;
  totalTokens: Knowledge<U64>;
  cost: Knowledge<{ amount: string; currency: string }>;
  accounting: "reported" | "estimated";
  nativeFieldsRef: Id | null;
};

export type ObservationPayload =
  | MessagePayload
  | ThoughtPayload
  | ToolCallPayload
  | ToolResultPayload
  | { interaction: Interaction }
  | { interactionId: Id; requestVersion: U64; answerCommandId: Id; actor: unknown; answerRef: Id; delivery: string }
  | { interactionId: Id; requestVersion: U64; reason: string; evidenceEventIds: Id[] }
  | WorkflowRunPayload
  | WorkflowPhasePayload
  | WorkflowMemberPayload
  | { type: "entity"; entityType: string; entityId: Id; revision: U64; previousState: string | null; state: string; reasonCode: string; evidenceEventIds: Id[]; entity: unknown }
  | UsagePayload
  | { nativeType: string; reason: string; rawRef: RawRef; affects: string[]; summary: string | null };

export type Observation = {
  schemaVersion: 1;
  eventId: Id;
  journalId: Id;
  instanceId: Id;
  runId: Id | null;
  hostId: Id;
  processGeneration: U64;
  runGeneration: U64 | null;
  seq: U64;
  observedAt: Timestamp;
  nativeAt: Knowledge<Timestamp>;
  source: ObservationSource;
  kind: ObservationKind;
  completeness: Completeness;
  rawRef: RawRef | null;
  evidenceEventIds: Id[];
  payload: ObservationPayload;
};

export type Snapshot = {
  projectionVersion: string;
  projectionEpoch: Id;
  asOfSeq: U64;
  instance: unknown;
  runs: unknown[];
  commands: unknown[];
  pendingInteractions: Interaction[];
  nodes: unknown[];
  history: { earliestRetainedSeq: U64; complete: boolean };
};

export type EventsBatch = {
  jsonrpc: "2.0";
  method: "events.batch";
  params: {
    subscriptionId: Id;
    journalId: Id;
    fromSeq: U64;
    toSeq: U64;
    events: Observation[];
    durableSeq: U64;
  };
};
