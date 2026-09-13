import type { Digest, Id, Knowledge, U64 } from "./wire";

/** Highest signal layer a live session reached; protocol §1.3 (D-028 §4.3). */
export type SignalTier = "hook" | "file" | "osc" | "screen" | "none";

/** One capability a live session reports, with the tier proving it; §1.3. */
export type RuntimeCapability = {
  name: CapabilityName;
  state: Capability["state"];
  tier: SignalTier;
  reasonCode: string;
};

export type NativeRef = {
  hostId: Id;
  nativeStoreId: Id;
  kind: "claude" | "codex" | "grok" | "agy" | "generic" | "terminal";
  sessionId: Knowledge<string>;
  transcript: Knowledge<{ objectId: Id; sourcePath: string }>;
  /** Absent means "nobody reported"; the static driver matrix still applies. */
  signalTier?: SignalTier;
  /** Runtime capability report. Outranks the matrix per name when present. */
  capabilities?: RuntimeCapability[];
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
  | "queue"
  | "interrupt"
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
  | "generic-pty"
  | "shell-pty";

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
