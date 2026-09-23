import type { CapabilitySnapshot, DriverKind, NativeRef, ProcessRef } from "./nativeRef";
import type { EntityMeta, Id, Knowledge, Timestamp, U64 } from "./wire";
import type { RegisteredWorkspace } from "./workspace";
import type { UsageRollup } from "../features/session/contextUsage";

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
export type InstanceMode = "native" | "promoted";
export type TuiMode = "fullscreen" | "default";

export type Instance = EntityMeta & {
  hostId: Id;
  workspaceId: Id;
  /**
   * Ledger task this session belongs to (`instances.task_id`, D-050). A task
   * aggregates its sessions for the task list; null for unbound sessions.
   * Additive wire field — the Hub already serves it on InstanceRecord.
   */
  taskId?: Id | null;
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
  providerSource?: string | null;
  providerSourceHint?: string | null;
  /**
   * Model-API route this instance actually uses (D-047). Absent on a direct
   * session. Reported by the Node, never derived from the request, so the
   * Session strip shows the route that ran rather than the one that was asked
   * for (D-035).
   */
  apiRoute?: {
    mode: "direct" | "via";
    route?: "direct-net" | "hub-relay" | null;
    viaHostId?: string | null;
    viaHostLabel?: string | null;
  } | null;
  model?: string | null;
  /** Requested launch renderer. Actual terminal mode comes from the tty snapshot. */
  tui?: TuiMode | null;
  /** Normalized D-028 §9.1 level (`low` … `max`). Legacy tiers are mapped by name on read. */
  effortName?: string | null;
  /** Dynamic-workflow flag; session-only, not a sixth level. */
  effortUltracode?: boolean | null;
  /** Legacy index. Preserved for older clients; never used to derive the tier. */
  effortIndex?: number | null;
  /** §9.1 effective effort read back from the native transcript; null = unobserved → UI shows `?`. */
  effortEffective?: {
    name: string;
    ultracode?: boolean | null;
    source: "launch" | "slash" | "remuda" | "unknown";
    observedAt: string;
  } | null;
  /** §9.1 effective model read back from the /model verdict / message.model. */
  modelEffective?: {
    id: string;
    source: "launch" | "slash" | "remuda" | "unknown";
    observedAt: string;
    /** Whether a Remuda switch used the session's own list or typed the id. */
    selectionPath?: "listed" | "typed" | null;
  } | null;
  /** model-pin-1 §5.4: launch model-pin divergences the Hub projected from the
   *  Node's `model_pin_mismatch` diagnostics. Durable — survives the bounded
   *  journal tail — and verbatim; the client renders, never recomputes. */
  modelPinMismatches?: {
    requested: string;
    observed: string;
    observedAt: string;
  }[] | null;
  /** §9.1 discovered switchable model list (gateway cache / settings / builtin). */
  modelCatalog?: {
    models: string[];
    source: "gateway-discovery" | "settings" | "builtin";
    observedAt: string;
    cache?: {
      scope: "scoped-config-dir" | "host-fallback";
      baseUrl?: string | null;
      fetchedAt?: string | null;
    } | null;
    discoveryEnv?: boolean | null;
  } | null;
  /** Effective permission mode read back from the TUI status line /
   *  transcript; absent = unobserved. The chip renders from this. */
  permissionEffective?: {
    mode: string;
    source: "launch" | "slash" | "remuda" | "unknown";
    observedAt: string;
  } | null;
  /** How this instance reached its `kind`; `promoted` = a terminal that an agent CLI took over (D-025). */
  mode?: InstanceMode | null;
  /** When the promotion happened. Only set while `mode` is `promoted`. */
  promotedAt?: string | null;
  /** Who ran the launch command (D-028 §1.0 rule 4). Provenance only — never a capability level. */
  launchedBy?: "remuda" | "user" | null;
  /** context-usage-1: additive per-session token/context rollup from the Hub; absent until the harness reports usage. */
  usageRollup?: UsageRollup | null;
};

export type HostTransport = "outbound-wss" | "ssh-dev";

export type HostCliAuth = "logged_in" | "logged_out" | "unknown" | "gateway-native" | "none";

export type HostCli = {
  kind: string;
  version?: string;
  path?: string;
  auth?: HostCliAuth;
  nativeGateway?: boolean;
  installed?: boolean;
};

export type Host = EntityMeta & {
  workspaces?: RegisteredWorkspace[];
  workspaceRevision?: number;
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
  resources?: { cpuPct?: number; memPct?: number; sampledAt?: string };
  herdr?: { version?: string; socket?: string; path?: string };
  nodeVersion?: string;
  instanceCount?: number;
  /** `auto` | `native` | `profile:<id>` */
  providerBinding?: string;
  /** Per-host default extra CLI args, used when a create omits `args`. */
  defaultLaunchArgs?: string[];
  /** Per-host Claude renderer default; an omitted value means fullscreen. */
  defaultTui?: TuiMode;
  /** Per-host default claude executable. Validated by the Node, not the Hub. */
  claudeBinaryPath?: string;
  online?: boolean;
  /** Raw Node-reported capability object (may carry `driverInventory`; D-028 §5.1). */
  capabilities?: Record<string, unknown> | null;
  /** Driver descriptors parsed out of {@link capabilities}, when the Node reported any. */
  driverInventory?: Array<{
    kind: string;
    launchable?: boolean;
    [key: string]: unknown;
  }>;
};

export type UiStatus = "blocked" | "working" | "starting" | "idle" | "exited" | "unknown";
