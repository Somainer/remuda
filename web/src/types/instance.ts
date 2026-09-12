import type { CapabilitySnapshot, DriverKind, NativeRef, ProcessRef } from "./nativeRef";
import type { EntityMeta, Id, Knowledge, Timestamp, U64 } from "./wire";

export type Kind = "claude" | "codex" | "grok" | "agy" | "generic";

export type Lifecycle =
  | "requested"
  | "preparing"
  | "starting"
  | "ready"
  | "closing"
  | "exited"
  | "failed"
  | "unknown"
  | "reconciling";

export type Activity = "idle" | "working" | "waiting-interaction" | "draining";
export type Connectivity = "connected" | "disconnected" | "reconciling";
export type Ownership = "managed" | "adopted-control" | "observed-only";
export type UiMode = "structured-only" | "tty-attachable";

export type Instance = EntityMeta & {
  hostId: Id;
  workspaceId: Id;
  kind: Kind;
  driver: DriverKind;
  lifecycle: Lifecycle;
  activity: Knowledge<Activity>;
  connectivity: Connectivity;
  ownership: Ownership;
  nativeRef: NativeRef;
  processRef: ProcessRef;
  specRevision: U64;
  launchId: Knowledge<Id>;
  capabilities: CapabilitySnapshot;
  ownerFence: U64;
  activeRunIds: Id[];
  parent: { instanceId: Id; runId: Id; commandId: Id } | null;
  journalId: Id;
  durableSeq: U64;
  exit: Knowledge<{ code: number | null; signal: string | null; observedAt: Timestamp }>;
  activityEvidenceEventIds?: Id[];
};

export type HostTransport = "outbound-wss" | "ssh-dev";

export type Host = EntityMeta & {
  label: string;
  ownerPrincipalId: Id;
  state: "enrolled" | "online" | "offline" | "reconciling" | "retired";
  transport: { mode: HostTransport; endpointRef: Id };
  hostname?: string;
  port?: number;
};

export type UiStatus = "blocked" | "working" | "starting" | "idle" | "exited" | "unknown";
