import type { CapabilitySnapshot, DriverKind, NativeRef, ProcessRef } from "./nativeRef";
import type { EntityMeta, Id, Knowledge, Timestamp, U64 } from "./wire";

export type Kind = "claude" | "codex" | "grok" | "agy" | "generic" | "terminal";

export type Lifecycle =
  | "requested"
  | "preparing"
  | "starting"
  | "ready"
  | "running"
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
  lastError?: string | null;
  activityEvidenceEventIds?: Id[];
  cwd?: string | null;
  name?: string | null;
  delegation?: string | null;
  providerProfileId?: string | null;
};

export type HostTransport = "outbound-wss" | "ssh-dev";

export type HostCliAuth = "logged_in" | "logged_out" | "unknown";

export type HostCli = {
  kind: string;
  version?: string;
  path?: string;
  auth?: HostCliAuth;
};

export type Host = EntityMeta & {
  label: string;
  ownerPrincipalId: Id;
  state: "enrolled" | "connecting" | "online" | "offline" | "reconciling" | "retired";
  ssh?: { workspaceRoot?: string; target: string; remudaBinaryPolicy: "require_installed" | "upload_if_missing" };
  lastError?: string;
  transport: { mode: HostTransport; endpointRef: Id };
  hostname?: string;
  port?: number;
  lastSeenAt?: string;
  cli?: HostCli[];
  labels?: string[];
  maxInstances?: number;
  resources?: { cpuPct?: number; memPct?: number };
  herdr?: { version?: string; socket?: string; path?: string };
  nodeVersion?: string;
  instanceCount?: number;
  online?: boolean;
};

export type UiStatus = "blocked" | "working" | "starting" | "idle" | "exited" | "unknown";
