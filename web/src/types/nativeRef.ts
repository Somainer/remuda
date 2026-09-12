import type { Digest, Id, Knowledge, U64 } from "./wire";

export type NativeRef = {
  hostId: Id;
  nativeStoreId: Id;
  kind: "claude" | "codex" | "grok" | "agy" | "generic";
  sessionId: Knowledge<string>;
  transcript: Knowledge<{ objectId: Id; sourcePath: string }>;
  codex?: { threadId: string };
  acp?: { sessionId: string; protocolVersion: number };
  claude?: { sessionId: string; backgroundJobId?: string };
  agy?: { conversationId: string };
  herdr?: { serverIdentity: Id; serverEpoch: Id; paneId: string };
};

export type ProcessRef = {
  processGeneration: U64;
  processIdentity: Knowledge<{ pid: number; birthId: string; supervisorId: Id }>;
  connectionEpoch: Id;
};

export type NativeRequestKey =
  | { type: "rpc"; valueType: "string" | "number"; value: string }
  | { type: "hook"; invocationId: Id }
  | { type: "none" };

export type CapabilityName =
  | "resume"
  | "steer"
  | "model-switch"
  | "fork"
  | "structured-workflow"
  | "artifact"
  | "tty-attach"
  | "hooks"
  | "interactive-approval"
  | "question"
  | "plan-review"
  | "elicitation"
  | "live-attach"
  | "completion-native-turn"
  | "completion-task";

export type Capability = {
  state: "supported" | "unsupported" | "unknown";
  scope: string[];
  reasonCode: string;
  prerequisites: string[];
  evidence: {
    type: "fixture" | "native-negotiation" | "source" | "help";
    ref: string;
    digest: Knowledge<Digest>;
  }[];
};

export type DriverKind =
  | "claude-print"
  | "claude-pty"
  | "claude-bg"
  | "codex-appserver"
  | "grok-acp"
  | "agy-print"
  | "generic-pty";

export type CapabilitySnapshot = {
  id: Id;
  driverKind: DriverKind;
  adapterVersion: string;
  binaryVersion: string;
  binaryDigest: Digest;
  nativeProtocolVersion: Knowledge<string>;
  settingsRevision: U64;
  providerProfileRevision: U64;
  capabilities: Record<CapabilityName, Capability>;
};
