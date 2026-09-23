import type { Command, CommandResult, Page } from "../types/command";
import type { Host, HostCli, Instance, TuiMode } from "../types/instance";
import type { Interaction, InteractionAnswer } from "../types/interaction";
import type { EventsBatch, Observation, Snapshot } from "../types/observation";
import { known, unknownKnowledge, type Id, type U64 } from "../types/wire";
import type { PromptMode } from "../types/generated";
import type { Workspace, WorkspaceSnapshot } from "../types/workspace";
import { mapWorkspace } from "../features/workspaces/registry";
import { followWorkspaces } from "../features/workspaces/follow";
import type {
  ProviderCreate,
  ProviderDiscoverBody,
  ProviderModel,
  ProviderModelInput,
  ProviderPatch,
  ProviderTestResult,
} from "../features/providers";
import { PROVIDER_PROFILES } from "../features/providers/fixtures";
import { coerceUsageRollup } from "../features/session/contextUsage";
import type { components, paths } from "./api.generated";
import { printCapabilities, ptyCapabilities, agentPtyCapabilities } from "./capabilities";
import type { JournalRead } from "./journal";
import {
  mockClose,
  mockCancel,
  mockConfigure,
  mockCreate,
  mockDb,
  mockDelete,
  mockDeviceList,
  mockDeviceRevoke,
  mockFleetBroadcast,
  mockHostName,
  mockKeys,
  mockLogin,
  mockPage,
  mockPairCode,
  mockPairRedeem,
  mockPasskeyDelete,
  mockPasskeyList,
  mockPasskeyLoginFinish,
  mockPasskeyLoginStart,
  mockPasskeyRegisterFinish,
  mockPasskeyRegisterStart,
  mockPasskeyRename,
  mockReadJournal,
  mockRespond,
  mockResume,
  mockScreenRead,
  mockSend,
  mockSnapshot,
  mockWorkspaceLabel,
} from "./mock";
import { digestPlaceholder, id, now } from "./ids";
import { parseScreenBody, type ScreenRead } from "./screen";
import type { AttachmentRef } from "./attachments";
import { HubHttpError } from "./httpError";
import { readSession, type DeviceSession, type PairCode, type PairedDevice } from "./session";
import { coerceObservation, coerceObservationList } from "./hubJournal";

/**
 * `DELETE /v1/instances/{id}`. The Hub record is gone whatever `nodePurge`
 * says; anything other than `purged` means the Node kept its data directory.
 */
export type InstanceDeleted = components["schemas"]["InstanceDeleted"];

export const MOCK = import.meta.env.VITE_MOCK === "1";

/** 200-JSON for a documented Hub REST operation. */
export type HubJson<Path extends keyof paths, Method extends keyof paths[Path]> = paths[Path][Method] extends {
  responses: { 200: { content: { "application/json": infer R } } };
}
  ? R
  : never;

/** JSON request body for a documented Hub REST operation. */
export type HubBody<Path extends keyof paths, Method extends keyof paths[Path]> = paths[Path][Method] extends {
  requestBody: { content: { "application/json": infer B } };
}
  ? B
  : paths[Path][Method] extends {
        requestBody?: { content: { "application/json": infer B } };
      }
    ? B
    : never;

const HUB_CAPABILITIES = printCapabilities();
const KINDS: Instance["kind"][] = ["claude", "codex", "grok", "agy", "generic", "terminal"];
const DRIVERS: Instance["driver"][] = [
  "claude-print",
  "claude-sdk",
  "claude-pty",
  "claude-bg",
  "codex-appserver",
  "grok-acp",
  "agy-print",
  "generic-pty",
  "shell-pty",
];
const LIFECYCLES: Instance["lifecycle"][] = [
  "requested",
  "preparing",
  "starting",
  "ready",
  "running",
  "closing",
  "exited",
  "failed",
  "unknown",
  "reconciling",
];
const ACTIVITIES = ["idle", "working", "waiting-interaction", "draining"] as const;
const HOST_STATES: Host["state"][] = ["enrolled", "connecting", "online", "offline", "reconciling", "retired"];

function mapKind(raw: string): Instance["kind"] {
  return KINDS.find((k) => k === raw) ?? "generic";
}

/**
 * Coerce a reported driver to a known one.
 *
 * The fallback is a display-only last resort for a driver this build has never
 * heard of. It must never swallow a driver we *do* know: mislabelling a
 * `claude-sdk` instance as `claude-print` tells the operator the session ends
 * after one turn when in fact its child is still alive across turns, which is
 * the whole difference between the two carriers (D-037).
 */
export function mapDriver(raw: string): Instance["driver"] {
  return DRIVERS.find((d) => d === raw) ?? "claude-print";
}

function mapLifecycle(raw: string): Instance["lifecycle"] {
  if (raw === "running") return "running";
  if (raw === "ready") return "ready";
  return LIFECYCLES.find((s) => s === raw) ?? "unknown";
}

function mapHostCli(raw: { kind?: unknown; version?: unknown; path?: unknown; auth?: unknown }): HostCli {
  const auth =
    raw.auth === "logged_in" ||
    raw.auth === "logged_out" ||
    raw.auth === "unknown" ||
    raw.auth === "gateway-native" ||
    raw.auth === "none"
      ? raw.auth
      : "unknown";
  const nativeGateway = "nativeGateway" in raw && raw.nativeGateway === true;
  return {
    kind: typeof raw.kind === "string" ? raw.kind : "unknown",
    version: typeof raw.version === "string" ? raw.version : undefined,
    path: typeof raw.path === "string" ? raw.path : undefined,
    auth,
    nativeGateway: nativeGateway || auth === "gateway-native" ? true : undefined,
    installed: "installed" in raw && raw.installed === false ? false : true,
  };
}

function mapActivity(raw: string | undefined): Instance["activity"] {
  if (raw === "blocked" || raw === "waiting-interaction") return known("waiting-interaction");
  const activity = ACTIVITIES.find((a) => a === raw);
  return activity ? known(activity) : unknownKnowledge(raw ?? "unknown");
}

function mapHost(h: components["schemas"]["HostView"]): Host {
  const id = (h.id ?? h.hostId) as Id;
  const state = HOST_STATES.find((s) => s === h.state) ?? (h.online ? "online" : "offline");
  const transport = h.transport === "ssh-dev" || h.transport === "ssh-stdio" ? "ssh-dev" : "outbound-wss";
  const cli = Array.isArray(h.cli) ? h.cli.map(mapHostCli) : [];
  const resources = h.resources && typeof h.resources === "object" ? h.resources : undefined;
  const herdr = h.herdr && typeof h.herdr === "object" ? h.herdr : undefined;
  // D-028 §5.1: the Node stores the driver inventory verbatim under
  // `capabilities`; the New Session matrix reads it rather than hardcoding
  // which drivers a host can launch.
  const rawCaps = h.capabilities && typeof h.capabilities === "object" ? h.capabilities : undefined;
  const driverInventory = Array.isArray((rawCaps as { driverInventory?: unknown } | undefined)?.driverInventory)
    ? ((rawCaps as { driverInventory: unknown[] }).driverInventory as Host["driverInventory"])
    : undefined;
  return {
    id,
    revision: "1",
    createdAt: h.lastSeenAt ?? "",
    updatedAt: h.lastSeenAt ?? "",
    label: h.label || h.name || id,
    workspaces: h.workspaces ?? [],
    workspaceRevision: h.workspaceRevision ?? 0,
    ownerPrincipalId: "" as Id,
    state,
    transport: { mode: transport, endpointRef: id },
    hostname: h.hostname ?? undefined,
    lastSeenAt: h.lastSeenAt ?? undefined,
    cli,
    capabilities: rawCaps as Host["capabilities"],
    driverInventory,
    labels: h.labels ?? [],
    maxInstances: h.maxInstances ?? 8,
    resources: resources
      ? {
          cpuPct: typeof resources.cpuPct === "number" ? resources.cpuPct : undefined,
          memPct: typeof resources.memPct === "number" ? resources.memPct : undefined,
          sampledAt:
            typeof (resources as { sampledAt?: unknown }).sampledAt === "string"
              ? ((resources as { sampledAt: string }).sampledAt)
              : undefined,
        }
      : undefined,
    herdr: herdr
      ? {
          version: typeof herdr.version === "string" ? herdr.version : undefined,
          socket: typeof herdr.socket === "string" ? herdr.socket : undefined,
          path: typeof herdr.path === "string" ? herdr.path : undefined,
        }
      : undefined,
    nodeVersion: "nodeVersion" in h && typeof h.nodeVersion === "string" ? h.nodeVersion : undefined,
    instanceCount: h.instanceCount ?? 0,
    online: h.online,
    ssh: h.ssh ?? undefined,
    lastError: h.lastError ?? undefined,
    providerBinding: typeof h.providerBinding === "string" && h.providerBinding ? h.providerBinding : "auto",
    defaultLaunchArgs: Array.isArray(h.defaultLaunchArgs) ? h.defaultLaunchArgs : undefined,
    claudeBinaryPath: h.claudeBinaryPath ?? undefined,
    defaultTui: h.defaultTui ?? undefined,
  };
}

export function mapInstance(rec: components["schemas"]["InstanceRecord"]): Instance {
  const id = rec.instanceId as Id;
  const hostId = rec.hostId as Id;
  const kind = mapKind(rec.kind);
  const driver = mapDriver(rec.driver);
  const extra = rec as components["schemas"]["InstanceRecord"] & {
    cwd?: string | null;
    name?: string | null;
    delegation?: string | null;
    taskId?: string | null;
    providerProfileId?: string | null;
    providerSource?: string | null;
    providerSourceHint?: string | null;
    model?: string | null;
    tui?: TuiMode | null;
    effortName?: string | null;
    effortIndex?: number | null;
    effortEffective?:
      | {
          name?: string;
          ultracode?: boolean | null;
          source?: string;
          observedAt?: string;
        }
      | null;
    modelEffective?:
      | { id?: string; source?: string; observedAt?: string }
      | null;
    modelCatalog?:
      | { models?: unknown; source?: string; observedAt?: string }
      | null;
    modelPinMismatches?:
      | readonly {
          requested?: unknown;
          observed?: unknown;
          observedAt?: unknown;
        }[]
      | null;
    mode?: string | null;
    promotedAt?: string | null;
    launchedBy?: string | null;
    signalTier?: string | null;
    lastError?: string | null;
    usageRollup?: unknown;
    apiRoute?: components["schemas"]["ApiRoute"] | null;
  };
  const ptyDriver = driver === "generic-pty" || driver === "claude-pty" || driver === "shell-pty";
  const capabilities =
    ptyDriver && kind !== "terminal"
      ? agentPtyCapabilities(kind, driver)
      : ptyDriver
        ? ptyCapabilities(driver)
        : HUB_CAPABILITIES;
  const launchedBy =
    extra.launchedBy === "remuda" || extra.launchedBy === "user" ? extra.launchedBy : null;
  const signalTier =
    extra.signalTier === "hook" ||
    extra.signalTier === "file" ||
    extra.signalTier === "osc" ||
    extra.signalTier === "screen" ||
    extra.signalTier === "none"
      ? extra.signalTier
      : undefined;
  return {
    id,
    revision: "1",
    createdAt: rec.createdAt ?? "",
    updatedAt: rec.updatedAt ?? "",
    hostId,
    workspaceId: (rec.workspaceId ?? extra.cwd ?? hostId) as Id,
    kind,
    driver,
    lifecycle: mapLifecycle(rec.lifecycle),
    activity: mapActivity(rec.activity),
    connectivity:
      rec.connectivity === "disconnected" || rec.connectivity === "reconciling" ? rec.connectivity : "connected",
    ownership: "managed",
    nativeRef: {
      hostId,
      nativeStoreId: id,
      kind,
      sessionId: unknownKnowledge("hub"),
      transcript: unknownKnowledge("hub"),
      ...(signalTier ? { signalTier } : {}),
    },
    processRef: {
      processGeneration: "1",
      processIdentity: unknownKnowledge("hub"),
      connectionEpoch: hostId,
    },
    specRevision: "1",
    launchId: unknownKnowledge("hub"),
    capabilities,
    ownerFence: "1",
    activeRunIds: [],
    parent: null,
    journalId: rec.journalId as Id,
    durableSeq: rec.durableSeq as U64,
    exit: { state: "not-applicable" },
    lastError: typeof extra.lastError === "string" ? extra.lastError : null,
    launchedBy,
    cwd: extra.cwd ?? rec.workspaceId ?? null,
    name: extra.name ?? rec.title ?? null,
    // D-050 task membership (instances.task_id); additive, absent on
    // unbound sessions and older rows.
    taskId: typeof extra.taskId === "string" && extra.taskId ? extra.taskId : null,
    delegation: typeof rec.delegation === "string" ? rec.delegation : extra.delegation ?? null,
    providerProfileId:
      typeof rec.providerProfileId === "string" ? rec.providerProfileId : extra.providerProfileId ?? null,
    providerSource:
      typeof rec.providerSource === "string" ? rec.providerSource : extra.providerSource ?? null,
    providerSourceHint:
      typeof rec.providerSourceHint === "string" ? rec.providerSourceHint : extra.providerSourceHint ?? null,
    model: typeof extra.model === "string" ? extra.model : null,
    tui: extra.tui === "fullscreen" || extra.tui === "default" ? extra.tui : null,
    effortName: typeof extra.effortName === "string" ? extra.effortName : null,
    effortIndex: typeof extra.effortIndex === "number" ? extra.effortIndex : null,
    effortEffective:
      extra.effortEffective && typeof extra.effortEffective.name === "string"
        ? {
            name: extra.effortEffective.name,
            ultracode:
              typeof extra.effortEffective.ultracode === "boolean"
                ? extra.effortEffective.ultracode
                : null,
            source:
              extra.effortEffective.source === "launch" ||
              extra.effortEffective.source === "slash" ||
              extra.effortEffective.source === "remuda"
                ? extra.effortEffective.source
                : "unknown",
            observedAt: extra.effortEffective.observedAt ?? "",
          }
        : null,
    modelEffective:
      extra.modelEffective && typeof extra.modelEffective.id === "string"
        ? {
            id: extra.modelEffective.id,
            source:
              extra.modelEffective.source === "launch" ||
              extra.modelEffective.source === "slash" ||
              extra.modelEffective.source === "remuda"
                ? extra.modelEffective.source
                : "unknown",
            observedAt: extra.modelEffective.observedAt ?? "",
          }
        : null,
    modelCatalog:
      extra.modelCatalog && Array.isArray(extra.modelCatalog.models)
        ? {
            models: extra.modelCatalog.models.filter(
              (m): m is string => typeof m === "string" && !!m,
            ),
            source:
              extra.modelCatalog.source === "gateway-discovery" ||
              extra.modelCatalog.source === "settings"
                ? extra.modelCatalog.source
                : "builtin",
            observedAt: extra.modelCatalog.observedAt ?? "",
          }
        : null,
    modelPinMismatches: Array.isArray(extra.modelPinMismatches)
      ? extra.modelPinMismatches.flatMap((row) =>
          typeof row?.requested === "string" &&
          typeof row.observed === "string" &&
          typeof row.observedAt === "string"
            ? [{ requested: row.requested, observed: row.observed, observedAt: row.observedAt }]
            : [],
        )
      : null,
    mode: extra.mode === "promoted" || extra.mode === "native" ? extra.mode : null,
    promotedAt: typeof extra.promotedAt === "string" ? extra.promotedAt : null,
    usageRollup: coerceUsageRollup(rec.usageRollup ?? extra.usageRollup),
    // D-047: the Node-echoed route only; a direct session omits it and the
    // requested route is never mapped here (D-035).
    apiRoute: extra.apiRoute
      ? {
          mode: extra.apiRoute.mode === "via" ? ("via" as const) : ("direct" as const),
          ...(extra.apiRoute.route ? { route: extra.apiRoute.route } : {}),
          ...(extra.apiRoute.viaHostId ? { viaHostId: extra.apiRoute.viaHostId as string } : {}),
          ...(extra.apiRoute.viaHostLabel ? { viaHostLabel: extra.apiRoute.viaHostLabel } : {}),
        }
      : null,
  };
}

function instanceTitle(rec: components["schemas"]["InstanceRecord"]): string | undefined {
  return rec.title ?? undefined;
}

function mapCommand(row: components["schemas"]["CommandRecord"], instanceId: Id): Command {
  const commandId = row.commandId as Id;
  const state: Command["state"] =
    row.state === "queued" || row.state === "accepted" || row.state === "settled" ? row.state : "accepted";
  const resolution: Command["resolution"] =
    row.resolution === "unknown" || row.resolution === "reconciling" ? row.resolution : "clear";
  return {
    id: commandId,
    revision: "1",
    createdAt: row.createdAt ?? "",
    updatedAt: row.updatedAt ?? "",
    commandId,
    actor: { principalId: "" as Id, type: "system", deviceId: null, instanceId },
    origin: "ui",
    operation: row.operation,
    target: {
      hostId: row.hostId as Id,
      instanceId: (row.instanceId ?? instanceId) as Id,
      runId: null,
    },
    payloadDigest: digestPlaceholder(),
    state,
    dispatch: row.forwarded ? "transport-written" : "intent-durable",
    resolution,
  };
}

function mapCommandResult(body: HubJson<"/v1/instances/{id}/commands", "post">, instanceId: Id): CommandResult {
  return { command: mapCommand(body.command, instanceId), relatedCommandIds: [] };
}

export type HelloResult = {
  protocol: { major: number; minor: number };
  connectionId: Id;
  serverEpoch: Id;
  observationSchemaMajor: 1;
  features: string[];
};

export type EffortRef = { index: number; name: string };

export type InstanceCreateSpec = {
  hostId: Id;
  workspaceId?: Id;
  kind: "claude" | "codex" | "grok" | "agy" | "terminal";
  driver: Instance["driver"];
  model: string;
  providerProfileId?: string;
  permissionMode: string;
  /** Codex sandbox mode (`read-only` / `workspace-write` / `danger-full-access`). */
  sandbox?: string;
  delegation?: "none" | "gateway";
  prompt: string;
  worktree?: boolean | string;
  cwd?: string;
  settingsOverlayPath?: string;
  claudeConfigDir?: string;
  maxBudgetUsd?: string;
  /** Extra native CLI args as an argv array, never a shell string. Replaces the host default. */
  args?: string[];
  /** Host-absolute claude executable. Validated and pinned by the Node, not the Hub. */
  binaryPath?: string;
  /** Initial Claude renderer, replacing the host default when present. */
  tui?: TuiMode;
  name?: string;
  /** Composer/New Session effort. Stored in UI state; Hub ignores unknown create fields. */
  effortIndex?: number;
  effortName?: string;
  /**
   * D-047 per-dispatch delivery override: a proxy host id, `self` for the Hub
   * host, or `none` to force direct. A target the Hub cannot honour refuses
   * with an `api-via-*` code; it never reroutes.
   */
  apiVia?: string;
  /** Route sub-mode for {@link InstanceCreateSpec.apiVia}. */
  apiRoute?: "auto" | "hub-relay" | "direct-net";
};

export type InstanceConfigurePatch = {
  permissionMode?: string;
  model?: string;
  effort?: EffortRef;
};

export type WorktreeRecord = {
  name: string;
  path: string;
  branch?: string;
  base?: string;
  hostId?: string;
};

export type WorktreePage = {
  items: WorktreeRecord[];
  hostId?: string;
  workspaceRoot?: string | null;
  nextCursor?: string | null;
};

export type WorktreeCreateSpec = {
  hostId?: string;
  workspaceId?: string;
  name: string;
  base?: string;
};

export type PtyKey = "enter" | "esc" | "ctrl+c";

/** Resume target: keep the structured transcript, or continue in a terminal (D-026). */
export type ResumeMode = "structured" | "terminal";

/** Where a resume landed: the new instance the caller should navigate to. */
export type ResumeResult = {
  instanceId: Id;
  mode: ResumeMode;
  /** True when an earlier resume for this target was reused. */
  replayed: boolean;
};

/** Hub GET `/v1/providers` row. Auth token is never present. */
export type HubProviderRow = {
  id: string;
  name: string;
  kind: "gateway" | "direct" | string;
  baseUrl: string;
  models: ProviderModelInput[];
  defaultModel?: string | null;
  headers?: Record<string, string>;
  defaultGateway: boolean;
  scope?: string;
  revision: string;
  secret: { present: boolean; last4?: string | null; fingerprint?: string | null };
  health?: { ok: boolean; checkedAt?: string | null; message?: string | null; status?: number | null; latencyMs?: number | null } | null;
  /** D-047 model-API delivery; absent reads as direct/auto. */
  delivery?: components["schemas"]["ProviderDelivery"];
  createdAt?: string;
  updatedAt?: string;
};

/** `POST /v1/fleet/broadcast` request body. */
export type FleetBroadcastBody = HubBody<"/v1/fleet/broadcast", "post">;
/** `POST /v1/fleet/broadcast` 200 body. */
export type FleetBroadcastResult = HubJson<"/v1/fleet/broadcast", "post">;
/** One instance's outcome inside a broadcast. */
export type FleetBroadcastEntry = NonNullable<FleetBroadcastResult["results"]>[number];

/** A registered passkey (`GET /v1/auth/passkeys` item). */
export type PasskeyView = components["schemas"]["Passkey"];

/** `{ challengeId, options }` envelope for a register/login ceremony. */
export type PasskeyEnvelope = { challengeId: string; options: Record<string, unknown> };

/** Browser attestation/assertion credential bodies for finish calls. */
export type PasskeyAttestationBody = {
  id: string;
  rawId: string;
  type: string;
  response: { attestationObject: string; clientDataJSON: string; transports?: string[] };
};
export type PasskeyAssertionBody = {
  id: string;
  rawId: string;
  type: string;
  response: { authenticatorData: string; clientDataJSON: string; signature: string; userHandle: string | null };
};

export type HubApi = {
  mock: boolean;
  login(bootstrapToken: string, deviceName: string): Promise<DeviceSession>;
  pairRedeem(code: string, deviceName: string): Promise<DeviceSession>;
  pairCode(): Promise<PairCode>;
  deviceList(): Promise<{ items: PairedDevice[] }>;
  deviceRevoke(deviceId: string): Promise<{ ok: boolean }>;
  passkeyRegisterStart(name: string): Promise<PasskeyEnvelope>;
  passkeyRegisterFinish(challengeId: string, attestation: PasskeyAttestationBody): Promise<PasskeyView>;
  passkeyLoginStart(mediation?: "conditional"): Promise<PasskeyEnvelope>;
  passkeyLoginFinish(challengeId: string, assertion: PasskeyAssertionBody, deviceName?: string): Promise<DeviceSession>;
  passkeyList(): Promise<{ items: PasskeyView[] }>;
  passkeyRename(passkeyId: string, name: string): Promise<PasskeyView>;
  passkeyDelete(passkeyId: string): Promise<{ ok: boolean }>;
  hasDeviceSession(): boolean;
  hello(): Promise<HelloResult>;
  instanceList(q?: { hostId?: string; workspaceId?: string; kind?: string }): Promise<Page<Instance>>;
  instanceGet(instanceId: Id): Promise<Instance>;
  instanceCreate(spec: InstanceCreateSpec): Promise<{ command: CommandResult["command"]; instance: Instance }>;
  instanceSend(
    instanceId: Id,
    prompt: string,
    attachments?: AttachmentRef[],
    mode?: PromptMode,
  ): Promise<CommandResult>;
  /** D-028 §5.3: interrupt the current turn; the process and session stay alive. */
  instanceCancel(instanceId: Id): Promise<CommandResult>;
  /** Stage one attachment for a later send (D-027/D-027b). Returns its id. */
  objectUpload(
    instanceId: Id,
    blob: Blob,
    mediaType: string,
    fileName?: string,
  ): Promise<{
    objectId: string;
    size: number;
    kind: "image" | "file";
    name: string | null;
    mediaType: string;
  }>;
  instanceKeys(instanceId: Id, key: PtyKey): Promise<CommandResult>;
  fleetBroadcast(body: FleetBroadcastBody): Promise<FleetBroadcastResult>;
  worktreeList(hostId?: string): Promise<WorktreePage>;
  worktreeCreate(spec: WorktreeCreateSpec): Promise<WorktreeRecord>;
  screenRead(instanceId: Id, lines?: number): Promise<ScreenRead>;
  instanceClose(instanceId: Id): Promise<CommandResult>;
  instanceResume(instanceId: Id, mode?: ResumeMode): Promise<ResumeResult>;
  /** `DELETE /v1/instances/{id}`; `force` stops a live Instance first. */
  instanceDelete(instanceId: Id, force?: boolean): Promise<InstanceDeleted>;
  instanceConfigure(instanceId: Id, permissionMode: string, extras?: InstanceConfigurePatch): Promise<CommandResult>;
  interactionList(q?: { instanceId?: Id; state?: string }): Promise<Interaction[]>;
  interactionGet(interactionId: Id): Promise<Interaction>;
  interactionRespond(interactionId: Id, answer: InteractionAnswer): Promise<CommandResult>;
  hostSshAdd(body: components["schemas"]["SshHostCreate"]): Promise<Host>;
  hostRemove(hostId: Id): Promise<void>;
  hostList(): Promise<Page<Host>>;
  hostGet(hostId: Id): Promise<Host>;
  hostPatch(hostId: Id, body: { name?: string; labels?: string[]; maxInstances?: number; providerBinding?: string; defaultLaunchArgs?: string[] | null; defaultTui?: TuiMode | null; claudeBinaryPath?: string | null }): Promise<Host>;
  workspaceList(hostId?: Id): Promise<Page<Workspace>>;
  workspaceRegister(hostId: Id, path: string): Promise<Page<Workspace> & { workspaceId?: string; workspaceRevision?: number }>;
  workspaceUnregister(hostId: Id, path: string): Promise<Page<Workspace> & { workspaceRevision?: number }>;
  hostWorkspaceSubscribe(onSnapshot: (snapshot: WorkspaceSnapshot) => void, refresh: () => void): () => void;
  providerList(q?: { hostId?: string }): Promise<{ items: HubProviderRow[]; nextCursor?: string | null }>;
  providerGet(id: string): Promise<HubProviderRow>;
  providerCreate(body: ProviderCreate): Promise<HubProviderRow>;
  providerPatch(id: string, body: ProviderPatch): Promise<HubProviderRow>;
  providerDelete(id: string): Promise<{ ok: boolean }>;
  providerTest(id: string): Promise<ProviderTestResult>;
  /** Probe a gateway's `/v1/models` before the profile is saved. */
  providerDiscover(body: ProviderDiscoverBody): Promise<ProviderTestResult>;
  eventsRead: JournalRead;
  eventsSubscribe(
    journalId: Id,
    afterSeq: U64 | null,
    onBatch: (batch: EventsBatch["params"]) => void,
    onGap?: (windowFloor: U64) => void,
  ): Promise<{
    subscriptionId: Id;
    journalId: Id;
    snapshot: Snapshot;
    /** Follow-snapshot window floor; null on an empty snapshot. */
    windowFromSeq: U64 | null;
    /** False when older rows exist below the snapshot window. */
    reachedAfterSeq: boolean;
    durableSeq: U64;
  }>;
  eventsAck(subscriptionId: Id, journalId: Id, throughSeq: U64): Promise<{ acknowledgedSeq: U64 }>;
  eventsUnsubscribe(subscriptionId: Id): Promise<void>;
  titleOf(instanceId: Id): string;
  summaryOf(instanceId: Id): string | undefined;
  permissionModeOf(instanceId: Id): string;
  hostName(hostId: Id): string;
  workspaceLabel(workspaceId: Id): string;
  disconnect(): void;
};

/** Live remuda-node / Hub origin. Default `remuda dev` is loopback :8787. */
function hubBase(): string {
  if (import.meta.env.DEV && import.meta.env.VITE_HUB_URL) return "";
  const raw = import.meta.env.VITE_API_BASE ?? import.meta.env.VITE_HUB_URL ?? "";
  return raw.replace(/\/$/, "");
}

function wsUrl(path: string): string {
  const base = hubBase();
  if (base.startsWith("https://")) return `${base.replace(/^https/, "wss")}${path}`;
  if (base.startsWith("http://")) return `${base.replace(/^http/, "ws")}${path}`;
  const proto = location.protocol === "https:" ? "wss" : "ws";
  return `${proto}://${location.host}${path}`;
}

function followUrl(instanceId: Id): string {
  const url = new URL(wsUrl("/v1/follow"));
  url.searchParams.set("instanceId", instanceId);
  return url.toString();
}

/** Authenticated Hub JSON request; shared by the api object and feature code. */
export async function rest<T>(path: string, req: RequestInit = {}): Promise<T> {
  const session = readSession();
  const headers = {
    "content-type": "application/json",
    // Allows a single-row migration of cookies issued before token indexing.
    ...(session ? { "X-Remuda-Device-Id": session.deviceId } : {}),
    ...(req.headers as Record<string, string> | undefined),
  };
  const res = await fetch(`${hubBase()}${path}`, {
    credentials: "include",
    ...req,
    headers,
  });
  if (!res.ok) {
    const text = await res.text();
    let code = `HTTP_${res.status}`;
    let message = text || `HTTP ${res.status}`;
    let reasons: string[] = [];
    let retryAfterMs: number | undefined;
    try {
      const body = JSON.parse(text) as {
        code?: string;
        error?: string;
        reasons?: unknown;
        retryAfterMs?: unknown;
      };
      if (body.code) code = body.code;
      if (body.error) message = body.error;
      if (typeof body.retryAfterMs === "number" && Number.isFinite(body.retryAfterMs)) {
        retryAfterMs = body.retryAfterMs;
      }
      if (Array.isArray(body.reasons)) {
        reasons = body.reasons.filter((reason): reason is string => typeof reason === "string" && reason.length > 0);
        if (reasons.length > 0) {
          message = [message, ...reasons].join(" · ");
        }
      }
    } catch {
      /* raw */
    }
    throw new HubHttpError(res.status, code, message, reasons, retryAfterMs);
  }
  if (res.status === 204) return undefined as T;
  return (await res.json()) as T;
}

/** Dummy URLs stand in for a gateway that is not listening. */
function mockUnreachable(baseUrl: string): boolean {
  return /127\.0\.0\.1:1|:1$|invalid|example/.test(baseUrl);
}

/** A fake gateway catalog so the mock exercises the discovery checklist. */
function mockDiscovered(baseUrl: string): ProviderModel[] {
  if (mockUnreachable(baseUrl)) return [];
  return [
    { id: "passthrough/auto", enabled: true, label: "Auto", contextWindow: 1_048_576, tags: ["1m"] },
    { id: "passthrough/auto_model", enabled: true, label: "Auto model" },
    { id: "passthrough/fast", enabled: true, label: "Fast", contextWindow: 200_000 },
  ];
}

function seedMockProviders(): HubProviderRow[] {
  return PROVIDER_PROFILES.filter((p) => p.kind !== "native").map((p) => ({
    id: p.id,
    name: p.name,
    kind: p.kind === "direct" ? "direct" : "gateway",
    baseUrl: p.baseUrl ?? "",
    models: p.models,
    defaultModel: p.defaultModel,
    headers: p.headers,
    defaultGateway: p.defaultGateway,
    scope: p.scope,
    revision: "1",
    secret: { present: p.secret.present, last4: p.secret.last4, fingerprint: p.secret.fingerprint },
    health: p.health,
    createdAt: "2026-09-12T00:00:00.000Z",
    updatedAt: "2026-09-12T00:00:00.000Z",
  }));
}

let mockProviders: HubProviderRow[] = seedMockProviders();

function mockProviderFingerprint(token: string): { last4: string; fingerprint: string } {
  return { last4: token.slice(-4), fingerprint: "0123456789abcdef" };
}

function createMockApi(): HubApi {
  const subs = new Map<Id, (batch: EventsBatch["params"]) => void>();
  return {
    mock: true,
    async login(bootstrapToken, deviceName) {
      return mockLogin(bootstrapToken, deviceName);
    },
    async pairRedeem(code, deviceName) {
      return mockPairRedeem(code, deviceName);
    },
    async pairCode() {
      return mockPairCode(readSession()?.token);
    },
    async deviceList() {
      return mockDeviceList(readSession()?.token);
    },
    async deviceRevoke(deviceId) {
      return mockDeviceRevoke(readSession()?.token, deviceId);
    },
    async passkeyRegisterStart(name) {
      return mockPasskeyRegisterStart(name);
    },
    async passkeyRegisterFinish(challengeId, attestation) {
      void challengeId;
      void attestation;
      const session = readSession();
      return mockPasskeyRegisterFinish(session?.deviceId ?? "dev_mock");
    },
    async passkeyLoginStart(mediation) {
      return mockPasskeyLoginStart(mediation);
    },
    async passkeyLoginFinish(challengeId, assertion, deviceName) {
      void challengeId;
      void assertion;
      return mockPasskeyLoginFinish(deviceName);
    },
    async passkeyList() {
      return mockPasskeyList(readSession()?.token);
    },
    async passkeyRename(passkeyId, name) {
      return mockPasskeyRename(readSession()?.token, passkeyId, name);
    },
    async passkeyDelete(passkeyId) {
      return mockPasskeyDelete(readSession()?.token, passkeyId);
    },
    hasDeviceSession() {
      return true;
    },
    async hello() {
      return {
        protocol: { major: 1, minor: 0 },
        connectionId: id("conn_"),
        serverEpoch: id("epoch_"),
        observationSchemaMajor: 1,
        features: ["snapshot-follow-v1"],
      };
    },
    async instanceList(q) {
      let items = mockDb.instances.slice();
      if (q?.hostId) items = items.filter((i) => i.hostId === q.hostId);
      if (q?.workspaceId) items = items.filter((i) => i.workspaceId === q.workspaceId);
      if (q?.kind) items = items.filter((i) => i.kind === q.kind);
      return mockPage(items);
    },
    async instanceGet(instanceId) {
      const found = mockDb.instances.find((i) => i.id === instanceId);
      if (!found) throw new Error("INSTANCE_NOT_FOUND");
      return found;
    },
    async instanceCreate(spec) {
      const workspaceId = (spec.workspaceId ?? spec.cwd ?? spec.hostId) as Id;
      const instance = mockCreate(spec.prompt, {
        hostId: spec.hostId,
        workspaceId,
        driver: spec.driver,
        kind: spec.kind,
      });
      instance.hostId = spec.hostId;
      instance.workspaceId = workspaceId;
      instance.cwd = spec.cwd ?? null;
      instance.delegation = spec.delegation ?? "none";
      instance.providerProfileId = spec.providerProfileId;
      instance.model = spec.model;
      instance.tui = spec.tui ?? mockDb.hosts.find((h) => h.id === spec.hostId)?.defaultTui ?? "fullscreen";
      if (spec.effortName != null) {
        instance.effortName = spec.effortName;
        instance.effortIndex = spec.effortIndex ?? 0;
      }
      return {
        instance,
        command: {
          id: instance.id,
          revision: "1",
          createdAt: now(),
          updatedAt: now(),
          commandId: id("cmd_"),
          actor: { principalId: id("prn_"), type: "human", deviceId: id("dev_"), instanceId: instance.id },
          origin: "ui",
          operation: "instance.create",
          target: { hostId: spec.hostId, instanceId: instance.id, runId: null },
          payloadDigest: digestPlaceholder(),
          state: "accepted",
          dispatch: "intent-durable",
          resolution: "clear",
        },
      };
    },
    async instanceSend(instanceId, prompt, _attachments, mode) {
      void mode;
      return mockSend(instanceId, prompt);
    },
    async instanceCancel(instanceId) {
      return mockCancel(instanceId);
    },
    async objectUpload(_instanceId, blob, mediaType, fileName) {
      // The mock Hub stages nothing; a deterministic id keeps the composer
      // exercisable offline.
      void mediaType;
      return {
        objectId: id("obj_"),
        size: blob.size,
        kind: mediaType.startsWith("image/") ? ("image" as const) : ("file" as const),
        name: fileName ?? null,
        mediaType,
      };
    },
    async instanceKeys(instanceId, key) {
      return mockKeys(instanceId, key);
    },
    async fleetBroadcast(body) {
      return mockFleetBroadcast(body ?? {});
    },
    async worktreeList() {
      return {
        items: mockDb.workspaces
          .filter((w) => w.worktreeLabel)
          .map((w) => ({
            name: w.worktreeLabel ?? w.label,
            path: w.rootPath,
            branch: w.branch,
            hostId: w.hostId,
          })),
        workspaceRoot: mockDb.workspaces[0]?.rootPath ?? "/",
      };
    },
    async worktreeCreate(spec) {
      return {
        name: spec.name,
        path: `/tmp/remuda-wt/${spec.name}`,
        branch: `wt/${spec.name}/work`,
        base: spec.base ?? "main",
        hostId: spec.hostId,
      };
    },
    async screenRead(instanceId, lines = 3) {
      return mockScreenRead(instanceId, lines);
    },
    async instanceClose(instanceId) {
      return mockClose(instanceId);
    },
    async instanceResume(instanceId, mode = "structured") {
      return mockResume(instanceId, mode);
    },
    async instanceDelete(instanceId, force) {
      return mockDelete(instanceId, force);
    },
    async instanceConfigure(instanceId, permissionMode, extras) {
      return mockConfigure(instanceId, extras?.permissionMode ?? permissionMode, extras);
    },
    async interactionList(q) {
      return mockDb.interactions.filter((i) => {
        if (q?.instanceId && i.instanceId !== q.instanceId) return false;
        if (q?.state && i.state !== q.state) return false;
        return true;
      });
    },
    async interactionGet(interactionId) {
      const found = mockDb.interactions.find((i) => i.id === interactionId);
      if (!found) throw new Error("INTERACTION_NOT_FOUND");
      return found;
    },
    async interactionRespond(interactionId, answer) {
      return mockRespond(interactionId, answer);
    },
    async hostSshAdd() { throw new Error("演示模式无法连接真实 SSH 主机"); },
    async hostRemove() { throw new Error("演示模式无法移除真实 SSH 主机"); },
    async hostList() {
      return mockPage(mockDb.hosts.map((host) => ({ ...host, workspaces: mockDb.workspaces
        .filter((w) => w.hostId === host.id).map((w) => ({ workspaceId: w.id, hostId: w.hostId, root: w.rootPath, worktreeLabel: w.worktreeLabel, branch: w.branch })) })));
    },
    async hostGet(hostId) {
      const found = mockDb.hosts.find((h) => h.id === hostId);
      if (!found) throw new Error("HOST_NOT_FOUND");
      return found;
    },
    async hostPatch(hostId, body) {
      const found = mockDb.hosts.find((h) => h.id === hostId);
      if (!found) throw new Error("HOST_NOT_FOUND");
      if (body.name) found.label = body.name;
      if (body.labels) found.labels = body.labels;
      if (body.maxInstances != null) found.maxInstances = body.maxInstances;
      if (body.providerBinding) found.providerBinding = body.providerBinding;
      // `null` clears; `undefined` means the PATCH did not mention the field.
      if (body.defaultLaunchArgs !== undefined) {
        found.defaultLaunchArgs = body.defaultLaunchArgs ?? undefined;
      }
      if (body.defaultTui !== undefined) {
        found.defaultTui = body.defaultTui ?? undefined;
      }
      if (body.claudeBinaryPath !== undefined) {
        found.claudeBinaryPath = body.claudeBinaryPath || undefined;
      }
      return found;
    },
    async providerList(q) {
      const items = mockProviders.filter((p) => {
        if (!q?.hostId) return true;
        const scope = p.scope ?? "universal";
        return scope === "universal" || scope === `host:${q.hostId}`;
      });
      return { items, nextCursor: null };
    },
    async providerGet(providerId) {
      const found = mockProviders.find((p) => p.id === providerId);
      if (!found) throw new Error("NOT_FOUND");
      return found;
    },
    async providerCreate(body) {
      if (!body.authToken) throw new Error("authToken is required on create");
      if (body.defaultGateway) mockProviders = mockProviders.map((p) => ({ ...p, defaultGateway: false }));
      const fp = mockProviderFingerprint(body.authToken);
      const row: HubProviderRow = {
        id: id("pvp_"),
        name: body.name,
        kind: body.kind,
        baseUrl: body.baseUrl,
        models: body.models,
        defaultModel: body.defaultModel ?? body.models.find((m) => m.enabled)?.id ?? null,
        headers: body.headers ?? {},
        defaultGateway: Boolean(body.defaultGateway && body.kind === "gateway"),
        scope: body.scope ?? "universal",
        revision: "1",
        secret: { present: true, last4: fp.last4, fingerprint: fp.fingerprint },
        health: null,
        createdAt: now(),
        updatedAt: now(),
      };
      mockProviders = [...mockProviders, row];
      return row;
    },
    async providerPatch(providerId, body) {
      const index = mockProviders.findIndex((p) => p.id === providerId);
      if (index < 0) throw new Error("NOT_FOUND");
      if (body.defaultGateway) mockProviders = mockProviders.map((p) => ({ ...p, defaultGateway: p.id === providerId }));
      const prev = mockProviders[index];
      const fp = body.authToken ? mockProviderFingerprint(body.authToken) : null;
      const row: HubProviderRow = {
        ...prev,
        name: body.name ?? prev.name,
        kind: body.kind ?? prev.kind,
        baseUrl: body.baseUrl ?? prev.baseUrl,
        models: body.models ?? prev.models,
        defaultModel: body.defaultModel === undefined ? prev.defaultModel : body.defaultModel,
        headers: body.headers ?? prev.headers,
        defaultGateway: body.defaultGateway ?? prev.defaultGateway,
        scope: body.scope ?? prev.scope,
        revision: String(Number(prev.revision) + 1),
        secret: fp ? { present: true, last4: fp.last4, fingerprint: fp.fingerprint } : prev.secret,
        updatedAt: now(),
      };
      mockProviders = mockProviders.map((p) => (p.id === providerId ? row : p));
      return row;
    },
    async providerDelete(providerId) {
      const before = mockProviders.length;
      mockProviders = mockProviders.filter((p) => p.id !== providerId);
      if (mockProviders.length === before) throw new Error("NOT_FOUND");
      return { ok: true };
    },
    async providerTest(providerId) {
      const found = mockProviders.find((p) => p.id === providerId);
      if (!found) throw new Error("NOT_FOUND");
      const dummy = /127\.0\.0\.1:1|:1$|invalid|example/.test(found.baseUrl);
      const result: ProviderTestResult = dummy
        ? { ok: false, reachable: false, message: `unreachable: connection refused (${found.baseUrl}/v1/models)`, models: [] }
        : { ok: true, reachable: true, status: 200, latencyMs: 12, message: "reachable (200); 1 models", models: mockDiscovered(found.baseUrl) };
      found.health = dummy
        ? { ok: false, message: result.message }
        : { ok: true, status: 200, latencyMs: 12, message: result.message, checkedAt: now() };
      return result;
    },
    async providerDiscover(body) {
      const saved = body.profileId ? mockProviders.find((p) => p.id === body.profileId) : undefined;
      if (body.profileId && !saved) throw new Error("NOT_FOUND");
      const baseUrl = body.baseUrl?.trim() || saved?.baseUrl || "";
      if (!baseUrl) throw new Error("gateway profiles require a baseUrl");
      if (mockUnreachable(baseUrl)) {
        return {
          ok: false,
          reachable: false,
          message: `unreachable: connection refused (${baseUrl}/v1/models)`,
          models: [],
        };
      }
      const models = mockDiscovered(baseUrl);
      return {
        ok: true,
        reachable: true,
        status: 200,
        latencyMs: 12,
        message: `reachable (200); ${models.length} models`,
        models,
      };
    },
    async workspaceList(hostId) {
      const items = hostId ? mockDb.workspaces.filter((w) => w.hostId === hostId) : mockDb.workspaces;
      return mockPage(items);
    },
    async workspaceRegister(hostId, path) {
      if (!path.startsWith("/")) throw new Error("Workspace path must be absolute");
      if (!mockDb.workspaces.some((w) => w.hostId === hostId && w.rootPath === path)) {
        mockDb.workspaces.push(mapWorkspace({ workspaceId: id("wsp_"), hostId, root: path }));
      }
      const page = await this.workspaceList(hostId);
      return { ...page, workspaceId: page.items.find((w) => w.rootPath === path)?.id };
    },
    async workspaceUnregister(hostId, path) {
      mockDb.workspaces = mockDb.workspaces.filter((w) => w.hostId !== hostId || w.rootPath !== path);
      return this.workspaceList(hostId);
    },
    hostWorkspaceSubscribe() { return () => undefined; },
    eventsRead: async ({ journalId, afterSeq, beforeSeq, limit }) => mockReadJournal(journalId, afterSeq, beforeSeq, limit),
    async eventsSubscribe(journalId, _afterSeq, onBatch, _onGap) {
      const instance = mockDb.instances.find((i) => i.journalId === journalId);
      if (!instance) throw new Error("JOURNAL_NOT_FOUND");
      const subscriptionId = id("sub_");
      subs.set(subscriptionId, onBatch);
      const snapshot = mockSnapshot(instance);
      let page: { events: Observation[]; durableSeq: string; windowFromSeq: U64 | null; reachedAfterSeq: boolean };
      try {
        page = mockReadJournal(journalId, snapshot.asOfSeq, undefined, 128);
      } catch {
        page = { events: [], durableSeq: snapshot.asOfSeq, windowFromSeq: null, reachedAfterSeq: true };
      }
      if (page.events.length) {
        queueMicrotask(() => {
          onBatch({
            subscriptionId,
            journalId,
            fromSeq: page.events[0].seq,
            toSeq: page.events[page.events.length - 1].seq,
            events: page.events,
            durableSeq: page.durableSeq,
          });
        });
      }
      return {
        subscriptionId,
        journalId,
        snapshot,
        windowFromSeq: page.windowFromSeq,
        reachedAfterSeq: page.reachedAfterSeq,
        durableSeq: snapshot.asOfSeq,
      };
    },
    async eventsAck(_subscriptionId, _journalId, throughSeq) {
      return { acknowledgedSeq: throughSeq };
    },
    async eventsUnsubscribe(subscriptionId) {
      subs.delete(subscriptionId);
    },
    titleOf(instanceId) {
      return mockDb.titles.get(instanceId) ?? "会话";
    },
    summaryOf(instanceId) {
      return mockDb.summaries.get(instanceId);
    },
    permissionModeOf(instanceId) {
      return mockDb.permissionMode.get(instanceId) ?? "manual";
    },
    hostName(hostId) {
      return mockDb.hosts.find((h) => h.id === hostId)?.label ?? mockHostName;
    },
    workspaceLabel(workspaceId) {
      return mockDb.workspaces.find((w) => w.id === workspaceId)?.label ?? mockWorkspaceLabel;
    },
    disconnect() {
      subs.clear();
    },
  };
}

function createLiveApi(): HubApi {
  const follows = new Map<Id, WebSocket>();
  const followSubs = new Map<Id, Id>();
  const titles = new Map<Id, string>();
  const hosts = new Map<Id, Host>();
  const workspaces = new Map<Id, Workspace>();
  const journals = new Map<Id, Id>();

  function remember(instance: Instance, title?: string): Instance {
    journals.set(instance.journalId, instance.id);
    journals.set(instance.id, instance.id);
    if (title) titles.set(instance.id, title);
    return instance;
  }

  function instanceIdOf(journalId: Id): Id {
    return journals.get(journalId) ?? journalId;
  }

  async function command(instanceId: Id, operation: string, payload: Record<string, unknown> = {}): Promise<CommandResult> {
    const req: HubBody<"/v1/instances/{id}/commands", "post"> = {
      operation,
      payload,
    };
    const result = await rest<HubJson<"/v1/instances/{id}/commands", "post">>(
      `/v1/instances/${instanceId}/commands`,
      { method: "POST", body: JSON.stringify(req) },
    );
    return mapCommandResult(result, instanceId);
  }

  return {
    mock: false,
    async login(bootstrapToken, deviceName) {
      const body = await rest<HubJson<"/v1/login", "post">>("/v1/login", {
        method: "POST",
        body: JSON.stringify({ bootstrapToken, deviceName }),
      });
      return body;
    },
    async pairRedeem(code, deviceName) {
      const body = await rest<HubJson<"/v1/devices/pair", "post">>("/v1/devices/pair", {
        method: "POST",
        body: JSON.stringify({ code, deviceName }),
      });
      return body;
    },
    async pairCode() {
      return rest<HubJson<"/v1/devices/pair-code", "post">>("/v1/devices/pair-code", {
        method: "POST",
        body: "{}",
      });
    },
    async deviceList() {
      return rest<HubJson<"/v1/devices", "get">>("/v1/devices");
    },
    async deviceRevoke(deviceId) {
      return rest<HubJson<"/v1/devices/{id}", "delete">>(`/v1/devices/${deviceId}`, { method: "DELETE" });
    },
    async passkeyRegisterStart(name) {
      return rest<HubJson<"/v1/auth/passkeys/register/start", "post">>(
        "/v1/auth/passkeys/register/start",
        { method: "POST", body: JSON.stringify({ name }) },
      ) as Promise<PasskeyEnvelope>;
    },
    async passkeyRegisterFinish(challengeId, attestation) {
      return rest<HubJson<"/v1/auth/passkeys/register/finish", "post">>(
        "/v1/auth/passkeys/register/finish",
        { method: "POST", body: JSON.stringify({ challengeId, attestation }) },
      );
    },
    async passkeyLoginStart(mediation) {
      const body = mediation ? JSON.stringify({ mediation }) : "{}";
      return rest<HubJson<"/v1/auth/passkeys/login/start", "post">>(
        "/v1/auth/passkeys/login/start",
        { method: "POST", body },
      ) as Promise<PasskeyEnvelope>;
    },
    async passkeyLoginFinish(challengeId, assertion, deviceName) {
      return rest<HubJson<"/v1/auth/passkeys/login/finish", "post">>(
        "/v1/auth/passkeys/login/finish",
        { method: "POST", body: JSON.stringify({ challengeId, assertion, deviceName }) },
      );
    },
    async passkeyList() {
      return rest<HubJson<"/v1/auth/passkeys", "get">>("/v1/auth/passkeys");
    },
    async passkeyRename(passkeyId, name) {
      return rest<HubJson<"/v1/auth/passkeys/{id}", "patch">>(`/v1/auth/passkeys/${passkeyId}`, {
        method: "PATCH",
        body: JSON.stringify({ name }),
      });
    },
    async passkeyDelete(passkeyId) {
      return rest<HubJson<"/v1/auth/passkeys/{id}", "delete">>(`/v1/auth/passkeys/${passkeyId}`, {
        method: "DELETE",
      });
    },
    hasDeviceSession() {
      // Persisted metadata is only a UI hint; the Hub validates the cookie.
      return Boolean(readSession());
    },
    async hello() {
      const health = await rest<{ ok?: boolean }>("/healthz");
      return {
        protocol: { major: 1, minor: 0 },
        connectionId: id("conn_"),
        serverEpoch: id("epoch_"),
        observationSchemaMajor: 1 as const,
        features: health.ok ? ["hub"] : [],
      };
    },
    async instanceList(q) {
      const page = await rest<HubJson<"/v1/instances", "get">>(
        `/v1/instances${q?.hostId ? `?hostId=${encodeURIComponent(q.hostId)}` : ""}`,
      );
      const items = page.items.map((row) => remember(mapInstance(row), instanceTitle(row)));
      return { items, nextCursor: page.nextCursor ?? null };
    },
    async instanceGet(instanceId) {
      const rec = await rest<components["schemas"]["InstanceRecord"]>(`/v1/instances/${instanceId}`);
      return remember(mapInstance(rec), instanceTitle(rec));
    },
    async instanceCreate(spec) {
      const worktreeName = typeof spec.worktree === "string" ? spec.worktree : undefined;
      const body: HubBody<"/v1/instances", "post"> = {
        hostId: spec.hostId,
        workspaceId: spec.workspaceId ?? spec.cwd,
        kind: spec.kind,
        driver: spec.driver,
        model: spec.model,
        providerProfileId: spec.providerProfileId,
        permissionMode: spec.permissionMode,
        delegation: spec.delegation,
        prompt: spec.prompt,
        name: spec.name,
        title: spec.name ?? spec.prompt.slice(0, 80),
        cwd: spec.cwd,
        worktree: worktreeName,
        settingsOverlayPath: spec.settingsOverlayPath,
        claudeConfigDir: spec.claudeConfigDir,
        maxBudgetUsd: spec.maxBudgetUsd,
        // Omitted rather than sent empty: an empty array would replace the
        // host default with "no args", which is a different request.
        args: spec.args?.length ? spec.args : undefined,
        binaryPath: spec.binaryPath || undefined,
        tui: spec.tui,
        // D-047 per-dispatch delivery override; omitted when the operator
        // leaves delivery to the profile/project waterfall.
        apiVia: spec.apiVia || undefined,
        apiRoute: spec.apiVia ? spec.apiRoute : undefined,
      };
      const created = await rest<HubJson<"/v1/instances", "post">>("/v1/instances", {
        method: "POST",
        body: JSON.stringify({
          ...body,
          ...(spec.effortName != null
            ? { effort: { name: spec.effortName, index: spec.effortIndex ?? 0, kind: spec.kind } }
            : {}),
        }),
      });
      const instance = remember(mapInstance(created.instance), spec.prompt.slice(0, 80) || spec.name);
      titles.set(instance.id, spec.prompt.slice(0, 80) || spec.name || "会话");
      return { instance, command: mapCommand(created.command, instance.id) };
    },
    async instanceSend(instanceId, prompt, attachments, mode) {
      const payload: Record<string, unknown> = { prompt };
      if (attachments?.length) payload.attachments = attachments;
      if (mode && mode !== "new-turn") payload.mode = mode;
      return command(instanceId, "instance.send", payload);
    },
    async instanceCancel(instanceId) {
      return command(instanceId, "instance.cancel", {});
    },
    async objectUpload(instanceId, blob, mediaType, fileName) {
      // Raw body plus Content-Type: no multipart, and the bytes never pass
      // through JSON. `rest` always sends application/json, so this posts
      // directly. The original filename rides the `name` query parameter;
      // the Hub sanitises it (D-027b).
      const session = readSession();
      const query = new URLSearchParams({ instanceId });
      if (fileName) query.set("name", fileName);
      const res = await fetch(`${hubBase()}/v1/objects?${query.toString()}`, {
        method: "POST",
        credentials: "include",
        headers: {
          "content-type": mediaType,
          ...(session ? { "X-Remuda-Device-Id": session.deviceId } : {}),
        },
        body: blob,
      });
      if (!res.ok) {
        const text = await res.text();
        let code = `HTTP_${res.status}`;
        let message = text || `HTTP ${res.status}`;
        try {
          const body = JSON.parse(text) as { code?: string; error?: string };
          if (body.code) code = body.code;
          if (body.error) message = body.error;
        } catch {
          /* raw */
        }
        throw new HubHttpError(res.status, code, message, []);
      }
      const body = (await res.json()) as {
        objectId: string;
        size: number;
        kind?: "image" | "file";
        name?: string | null;
        mediaType?: string;
      };
      return {
        objectId: body.objectId,
        size: body.size,
        kind: body.kind ?? "image",
        name: body.name ?? null,
        mediaType: body.mediaType ?? mediaType,
      };
    },
    async instanceKeys(instanceId, key) {
      return command(instanceId, "tty.write", { keys: [key], source: "ui" });
    },
    async fleetBroadcast(body) {
      return rest<FleetBroadcastResult>("/v1/fleet/broadcast", {
        method: "POST",
        body: JSON.stringify(body),
      });
    },
    async worktreeList(hostId) {
      const qs = hostId ? `?hostId=${encodeURIComponent(hostId)}` : "";
      return rest<WorktreePage>(`/v1/worktrees${qs}`);
    },
    async worktreeCreate(spec) {
      return rest<WorktreeRecord>("/v1/worktrees", { method: "POST", body: JSON.stringify(spec) });
    },
    async screenRead(instanceId, lines = 3) {
      try {
        const body = await rest<unknown>(`/v1/instances/${instanceId}/screen?lines=${lines}`);
        return parseScreenBody(body);
      } catch (err) {
        // The Hub refused this read to protect the control-RPC reservation
        // (NODE_BUSY/503): nothing failed on the Node and the next poll cycle
        // is expected to succeed. Surface it so the store backs off instead
        // of hammering.
        if (
          err instanceof HubHttpError &&
          (err.code === "NODE_BUSY" || err.status === 503)
        ) {
          // Honour the Hub's back-off hint; fall back to one poll cycle.
          throw new ScreenNodeBusyError(err.retryAfterMs ?? 2500);
        }
        // A 4xx means "no screen for this instance right now": host offline
        // (422 Unsatisfiable), unsupported carrier/404, etc. That is an
        // expected empty screen and the caller derives a journal fallback.
        // A 5xx or a network failure is an UNEXPECTED read failure: propagate
        // it so the store reports instead of silently showing fallback
        // content as a current screen.
        if (err instanceof HubHttpError && err.status < 500) {
          return { lines: [] };
        }
        throw err;
      }
    },
    async instanceClose(instanceId) {
      return command(instanceId, "instance.close", {});
    },
    async instanceResume(instanceId, mode = "structured") {
      const body = await rest<HubJson<"/v1/instances/{id}/resume", "post">>(
        `/v1/instances/${instanceId}/resume`,
        { method: "POST", body: JSON.stringify({ mode }) },
      );
      return {
        instanceId: body.instance.instanceId as Id,
        mode: body.mode,
        replayed: Boolean(body.replayed),
      };
    },
    async instanceDelete(instanceId, force) {
      // `force=1` is what stops a live Instance — the Hub stops and deletes
      // together, so the client never closes it separately. A repeated delete
      // answers 404, which is the session already being gone.
      try {
        return await rest<InstanceDeleted>(`/v1/instances/${instanceId}${force ? "?force=1" : ""}`, { method: "DELETE" });
      } catch (err) {
        if (err instanceof HubHttpError && err.status === 404) return { deleted: true, instanceId };
        throw err;
      }
    },
    async instanceConfigure(instanceId, permissionMode, extras) {
      return command(instanceId, "instance.configure", {
        permissionMode: extras?.permissionMode ?? permissionMode,
        ...(extras?.model ? { model: extras.model } : {}),
        ...(extras?.effort ? { effort: extras.effort } : {}),
      });
    },
    async interactionList(q) {
      const qs = new URLSearchParams();
      if (q?.instanceId) qs.set("instanceId", q.instanceId);
      const page = await rest<HubJson<"/v1/interactions", "get">>(`/v1/interactions${qs.size ? `?${qs}` : ""}`);
      const items = (page.items ?? []) as Interaction[];
      return items.filter((i) => {
        if (q?.instanceId && i.instanceId !== q.instanceId) return false;
        if (q?.state && i.state !== q.state) return false;
        return true;
      });
    },
    async interactionGet(interactionId) {
      const items = await this.interactionList();
      const found = items.find((i) => i.id === interactionId);
      if (!found) throw new Error("INTERACTION_NOT_FOUND");
      return found;
    },
    async interactionRespond(interactionId, answer) {
      const result = await rest<HubJson<"/v1/interactions/{id}/answer", "post">>(
        `/v1/interactions/${interactionId}/answer`,
        { method: "POST", body: JSON.stringify({ answer }) },
      );
      if (result && typeof result === "object" && "command" in result) {
        const found = await this.interactionGet(interactionId).catch(() => null);
        return mapCommandResult(result as HubJson<"/v1/instances/{id}/commands", "post">, found?.instanceId ?? ("" as Id));
      }
      return {
        command: {
          id: interactionId,
          revision: "1",
          createdAt: now(),
          updatedAt: now(),
          commandId: interactionId,
          actor: { principalId: "" as Id, type: "human", deviceId: null, instanceId: null },
          origin: "ui",
          operation: "interaction.respond",
          target: { hostId: "" as Id, instanceId: null, runId: null },
          payloadDigest: digestPlaceholder(),
          state: "accepted",
          dispatch: "transport-written",
          resolution: "clear",
        },
        relatedCommandIds: [],
      };
    },
    async hostSshAdd(body) {
      return mapHost(await rest<components["schemas"]["HostView"]>("/v1/hosts/ssh", { method: "POST", body: JSON.stringify(body) }));
    },
    async hostRemove(hostId) {
      await rest<void>(`/v1/hosts/${encodeURIComponent(hostId)}`, { method: "DELETE" });
      hosts.delete(hostId);
    },
    async hostList() {
      const page = await rest<HubJson<"/v1/hosts", "get">>("/v1/hosts");
      const items = page.items.map(mapHost);
      for (const h of items) hosts.set(h.id, h);
      return { items, nextCursor: page.nextCursor ?? null };
    },
    async hostGet(hostId) {
      const host = mapHost(await rest<HubJson<"/v1/hosts/{id}", "get">>(`/v1/hosts/${hostId}`));
      hosts.set(host.id, host);
      return host;
    },
    async hostPatch(hostId, body) {
      const host = mapHost(
        await rest<HubJson<"/v1/hosts/{id}", "patch">>(`/v1/hosts/${encodeURIComponent(hostId)}`, {
          method: "PATCH",
          body: JSON.stringify(body),
        }),
      );
      hosts.set(host.id, host);
      return host;
    },
    async providerList(q) {
      const suffix = q?.hostId ? `?hostId=${encodeURIComponent(q.hostId)}` : "";
      return rest<{ items: HubProviderRow[]; nextCursor?: string | null }>(`/v1/providers${suffix}`);
    },
    async providerGet(providerId) {
      return rest<HubProviderRow>(`/v1/providers/${providerId}`);
    },
    async providerCreate(body) {
      return rest<HubProviderRow>("/v1/providers", { method: "POST", body: JSON.stringify(body) });
    },
    async providerPatch(providerId, body) {
      return rest<HubProviderRow>(`/v1/providers/${providerId}`, { method: "PATCH", body: JSON.stringify(body) });
    },
    async providerDelete(providerId) {
      return rest<{ ok: boolean }>(`/v1/providers/${providerId}`, { method: "DELETE" });
    },
    async providerTest(providerId) {
      return rest<ProviderTestResult>(`/v1/providers/${providerId}/test`, { method: "POST", body: "{}" });
    },
    async providerDiscover(body) {
      return rest<ProviderTestResult>("/v1/providers/discover", { method: "POST", body: JSON.stringify(body) });
    },
    async workspaceList(hostId) {
      const listed = hostId ? [await this.hostGet(hostId)] : (await this.hostList()).items;
      const items = listed.flatMap((h) => (h.workspaces ?? []).map(mapWorkspace));
      for (const [key, value] of workspaces) if (!hostId || value.hostId === hostId) workspaces.delete(key);
      for (const w of items) workspaces.set(w.id, w);
      return { items, nextCursor: null };
    },
    async workspaceRegister(hostId, path) {
      const response = await rest<HubJson<"/v1/hosts/{id}/workspaces", "post">>(`/v1/hosts/${encodeURIComponent(hostId)}/workspaces`, {
        method: "POST", body: JSON.stringify({ path }),
      });
      return { items: response.workspaces.map(mapWorkspace), workspaceId: response.workspaceId, workspaceRevision: response.workspaceRevision, nextCursor: null };
    },
    async workspaceUnregister(hostId, path) {
      const response = await rest<HubJson<"/v1/hosts/{id}/workspaces", "delete">>(`/v1/hosts/${encodeURIComponent(hostId)}/workspaces`, {
        method: "DELETE", body: JSON.stringify({ path }),
      });
      return { items: response.workspaces.map(mapWorkspace), workspaceRevision: response.workspaceRevision, nextCursor: null };
    },
    hostWorkspaceSubscribe(onSnapshot, refresh) {
      return followWorkspaces(wsUrl("/v1/follow"), onSnapshot, refresh);
    },
    eventsRead: async (args) => {
      const instanceId = instanceIdOf(args.journalId);
      const qs = new URLSearchParams();
      if (args.afterSeq) qs.set("afterSeq", args.afterSeq);
      if (args.beforeSeq) qs.set("beforeSeq", args.beforeSeq);
      const suffix = qs.size ? `?${qs}` : "";
      const page = await rest<HubJson<"/v1/instances/{id}/journal", "get">>(
        `/v1/instances/${instanceId}/journal${suffix}`,
      );
      const events = coerceObservationList(page.events, args.journalId, instanceId).slice(0, args.limit);
      // Pre-window Hubs omit the metadata; their pages always covered the
      // whole range, so the safe defaults are "complete from the first row".
      const windowFromSeq = (page.fromSeq ?? events[0]?.seq ?? null) as U64 | null;
      const reachedAfterSeq = page.reachedAfterSeq ?? true;
      return {
        events,
        durableSeq: page.durableSeq as U64,
        windowFromSeq,
        reachedAfterSeq,
      };
    },
    async eventsSubscribe(journalId, afterSeq, onBatch, onGap) {
      const instanceId = instanceIdOf(journalId);
      follows.get(journalId)?.close();
      const subscriptionId = id("sub_") as Id;
      const ws = new WebSocket(followUrl(instanceId));
      follows.set(journalId, ws);
      followSubs.set(subscriptionId, journalId);
      const snapshotMeta: { fromSeq: U64 | null; reachedAfterSeq: boolean } = {
        fromSeq: null,
        reachedAfterSeq: true,
      };
      const asOfSeq = await new Promise<U64>((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error("FOLLOW_SNAPSHOT_TIMEOUT")), 10_000);
        let settled = false;
        const finish = (seq: U64) => {
          if (settled) return;
          settled = true;
          clearTimeout(timer);
          resolve(seq);
        };
        // Coalesce live `event` frames into contiguous batches on a trailing
        // macrotask. One frame = one store emit + O(n) transcript assembly,
        // so a burst (thousands of single-event frames) froze the tab for
        // minutes with an O(n^2) storm. Buffering makes the page cost
        // O(frames + batches * n) instead; ordering is preserved and a frame
        // flush is forced before every snapshot/other control frame.
        let pending: Observation[] = [];
        let flushScheduled = false;
        let gapArrived = false;
        const flushPending = () => {
          flushScheduled = false;
          if (!pending.length) return;
          // Split at seq discontinuities: JournalClient.applyBatch needs each
          // delivered batch to be a contiguous fromSeq..toSeq run.
          const runs: Observation[][] = [];
          let run: Observation[] = [];
          let expectSeq = -1;
          for (const ev of pending) {
            const seq = Number(ev.seq);
            if (run.length && seq !== expectSeq) {
              runs.push(run);
              run = [];
            }
            run.push(ev);
            expectSeq = seq + 1;
          }
          if (run.length) runs.push(run);
          pending = [];
          for (const group of runs) {
            onBatch({
              subscriptionId,
              journalId,
              fromSeq: group[0].seq,
              toSeq: group[group.length - 1].seq,
              events: group,
              durableSeq: group[group.length - 1].seq,
            });
          }
        };
        const scheduleFlush = () => {
          if (flushScheduled) return;
          flushScheduled = true;
          setTimeout(flushPending, 0);
        };
        ws.addEventListener("error", () => {
          if (settled) return;
          settled = true;
          clearTimeout(timer);
          reject(new Error("FOLLOW_CONNECT_FAILED"));
        });
        ws.addEventListener("message", (ev) => {
          if (typeof ev.data !== "string") return;
          let msg: { type?: string; asOfSeq?: string; durableSeq?: string; fromSeq?: string | null; reachedAfterSeq?: boolean; seq?: string; events?: unknown; event?: unknown };
          try {
            msg = JSON.parse(ev.data) as typeof msg;
          } catch {
            return;
          }
          if (msg.type === "event") {
            const obs = coerceObservation(msg.event ?? msg, journalId, instanceId, msg.seq);
            if (!obs) return;
            pending.push(obs);
            scheduleFlush();
            return;
          }
          // Force buffered live events out before a control frame so a
          // backpressure snapshot never overtakes the frames it follows.
          if (flushScheduled || pending.length) flushPending();
          if (msg.type === "gap") {
            // Hub-side backpressure; the bounded resync snapshot follows.
            // Remember the signal; the snapshot arm kicks the actual fill with
            // the hole's real seq bounds.
            gapArrived = true;
            return;
          }
          if (msg.type === "snapshot") {
            const events = coerceObservationList(msg.events, journalId, instanceId);
            const from = afterSeq ? events.filter((e) => Number(e.seq) > Number(afterSeq)) : events;
            if (from.length) {
              onBatch({
                subscriptionId,
                journalId,
                fromSeq: from[0].seq,
                toSeq: from[from.length - 1].seq,
                events: from,
                durableSeq: (msg.asOfSeq ?? from[from.length - 1].seq) as U64,
              });
            }
            // The follow snapshot is the same bounded window as GET journal:
            // its floor is a window floor and complete=false means older rows
            // remain available behind the load-earlier row.
            const snapshotFloor = (msg.fromSeq ?? from[0]?.seq ?? null) as U64 | null;
            const snapshotReached = msg.reachedAfterSeq ?? true;
            snapshotMeta.fromSeq = snapshotFloor;
            snapshotMeta.reachedAfterSeq = snapshotReached;
            // A preceding gap + a floor above the first delivered event
            // defines a hole the app must descend with beforeSeq.
            if (gapArrived && snapshotFloor) {
              onGap?.(snapshotFloor);
            }
            gapArrived = false;
            finish((msg.asOfSeq ?? afterSeq ?? "0") as U64);
          }
        });
      });
      return {
        subscriptionId,
        journalId,
        snapshot: {
          projectionVersion: "v1",
          projectionEpoch: id("epoch_"),
          asOfSeq,
          instance: {} as Instance,
          runs: [],
          commands: [],
          pendingInteractions: [],
          nodes: [],
          history: {
            earliestRetainedSeq: snapshotMeta.fromSeq ?? "1",
            complete: snapshotMeta.reachedAfterSeq,
          },
        },
        windowFromSeq: snapshotMeta.fromSeq,
        reachedAfterSeq: snapshotMeta.reachedAfterSeq,
        durableSeq: asOfSeq,
      };
    },
    async eventsAck(_subscriptionId, _journalId, throughSeq) {
      return { acknowledgedSeq: throughSeq };
    },
    async eventsUnsubscribe(subscriptionId) {
      const journalId = followSubs.get(subscriptionId);
      followSubs.delete(subscriptionId);
      if (!journalId) return;
      follows.get(journalId)?.close();
      follows.delete(journalId);
    },
    titleOf(instanceId) {
      return titles.get(instanceId) ?? "会话";
    },
    summaryOf() {
      return undefined;
    },
    permissionModeOf() {
      return "manual";
    },
    hostName(hostId) {
      return hosts.get(hostId)?.label ?? hostId.slice(0, 8);
    },
    workspaceLabel(workspaceId) {
      return workspaces.get(workspaceId)?.label ?? workspaceId.slice(0, 8);
    },
    disconnect() {
      for (const ws of follows.values()) ws.close();
      follows.clear();
    },
  };
}

/**
 * A bulk screen read the Hub refused under load (HTTP 503 `NODE_BUSY`).
 * Retryable by contract: the Hub reserves half the per-Node RPC budget for
 * control calls, so a saturated list must wait one poll cycle rather than
 * surface an error. The store catches this and backs off silently.
 */
export class ScreenNodeBusyError extends Error {
  readonly retryAfterMs: number;
  constructor(retryAfterMs = 2500) {
    super("node busy");
    this.name = "ScreenNodeBusyError";
    this.retryAfterMs = retryAfterMs;
  }
}

export function isScreenNodeBusy(err: unknown): err is ScreenNodeBusyError {
  return err instanceof ScreenNodeBusyError;
}

export const api: HubApi = MOCK ? createMockApi() : createLiveApi();

export interface HostDoctorReport {
  exitCode: number;
  checks: { name: string; status: string; message: string }[];
}

/** Fresh read-only checks run by the selected Node with its own process permissions. */
export async function fetchHostDoctor(hostId: Id): Promise<HostDoctorReport> {
  if (MOCK) return { exitCode: 0, checks: [{ name: "host.diagnostics", status: "warning", message: "模拟模式不运行主机诊断" }] };
  const report = await rest<unknown>(`/v1/hosts/${encodeURIComponent(hostId)}/doctor`, { signal: AbortSignal.timeout(65_000) });
  if (!report || typeof report !== "object" || !("exitCode" in report) || !Number.isInteger(report.exitCode)
    || !("checks" in report) || !Array.isArray(report.checks) || report.checks.length === 0
    || !report.checks.every((check: unknown) => check !== null && typeof check === "object"
      && "name" in check && typeof check.name === "string"
      && "status" in check && ["ok", "warning", "blocker"].includes(String(check.status))
      && "message" in check && typeof check.message === "string")) {
    throw new Error("主机返回了无效的诊断报告；请升级 Node 后重新检查");
  }
  return report as HostDoctorReport;
}

/** Uncompressed P-256 prefix + dummy coordinates; mock VAPID only. */
const MOCK_VAPID_PUBLIC_KEY =
  "BAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

const mockPushSubs = new Map<string, components["schemas"]["PushSubscribe"]>();

/** `GET /push/config` — VAPID public key for `PushManager.subscribe`. */
export async function fetchPushConfig(): Promise<HubJson<"/push/config", "get">> {
  if (MOCK) return { public_key: MOCK_VAPID_PUBLIC_KEY };
  return rest<HubJson<"/push/config", "get">>("/push/config");
}

/** `POST /push/subscriptions` — upsert browser PushSubscription JSON. */
export async function postPushSubscription(
  body: components["schemas"]["PushSubscribe"],
): Promise<{ ok?: boolean }> {
  if (MOCK) {
    mockPushSubs.set(body.endpoint, body);
    return { ok: true };
  }
  return rest<{ ok?: boolean }>("/push/subscriptions", { method: "POST", body: JSON.stringify(body) });
}

/** `DELETE /push/subscriptions?endpoint=` — drop that endpoint. */
export async function deletePushSubscription(endpoint: string): Promise<{ ok?: boolean }> {
  if (MOCK) {
    mockPushSubs.delete(endpoint);
    return { ok: true };
  }
  return rest<{ ok?: boolean }>(`/push/subscriptions?endpoint=${encodeURIComponent(endpoint)}`, { method: "DELETE" });
}

export function observationText(obs: Observation): string {
  const payload = obs.payload as { blocks?: { type: string; text?: string }[]; text?: string };
  if (typeof payload.text === "string") return payload.text;
  if (Array.isArray(payload.blocks)) {
    return payload.blocks
      .map((b) => (b.type === "text" ? b.text ?? "" : ""))
      .filter(Boolean)
      .join("\n");
  }
  return "";
}
