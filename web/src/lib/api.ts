import type { Command, CommandResult, Page } from "../types/command";
import type { Host, HostCli, Instance } from "../types/instance";
import type { Interaction, InteractionAnswer } from "../types/interaction";
import type { EventsBatch, Observation, Snapshot } from "../types/observation";
import { known, unknownKnowledge, type Id, type U64 } from "../types/wire";
import type { Workspace } from "../types/workspace";
import type { ProviderCreate, ProviderPatch, ProviderTestResult } from "../features/providers";
import { PROVIDER_PROFILES } from "../features/providers/fixtures";
import type { components, paths } from "./api.generated";
import { printCapabilities, ptyCapabilities } from "./capabilities";
import type { JournalRead } from "./journal";
import {
  mockClose,
  mockConfigure,
  mockCreate,
  mockDb,
  mockDeviceList,
  mockDeviceRevoke,
  mockFleetBroadcast,
  mockHostName,
  mockKeys,
  mockLogin,
  mockPage,
  mockPairCode,
  mockPairRedeem,
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
import { HubHttpError } from "./httpError";
import { readSession, type DeviceSession, type PairCode, type PairedDevice } from "./session";
import { coerceObservation, coerceObservationList } from "./hubJournal";

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

function mapDriver(raw: string): Instance["driver"] {
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
  return {
    id,
    revision: "1",
    createdAt: h.lastSeenAt ?? "",
    updatedAt: h.lastSeenAt ?? "",
    label: h.label || h.name || id,
    ownerPrincipalId: "" as Id,
    state,
    transport: { mode: transport, endpointRef: id },
    hostname: h.hostname ?? undefined,
    lastSeenAt: h.lastSeenAt ?? undefined,
    cli,
    labels: h.labels ?? [],
    maxInstances: h.maxInstances ?? 8,
    resources: resources
      ? {
          cpuPct: typeof resources.cpuPct === "number" ? resources.cpuPct : undefined,
          memPct: typeof resources.memPct === "number" ? resources.memPct : undefined,
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
  };
}

function mapInstance(rec: components["schemas"]["InstanceRecord"]): Instance {
  const id = rec.instanceId as Id;
  const hostId = rec.hostId as Id;
  const kind = mapKind(rec.kind);
  const driver = mapDriver(rec.driver);
  const extra = rec as components["schemas"]["InstanceRecord"] & {
    cwd?: string | null;
    name?: string | null;
    delegation?: string | null;
    providerProfileId?: string | null;
    providerSource?: string | null;
    providerSourceHint?: string | null;
    model?: string | null;
    effortName?: string | null;
    effortIndex?: number | null;
  };
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
    },
    processRef: {
      processGeneration: "1",
      processIdentity: unknownKnowledge("hub"),
      connectionEpoch: hostId,
    },
    specRevision: "1",
    launchId: unknownKnowledge("hub"),
    capabilities:
      driver === "generic-pty" || driver === "claude-pty" || driver === "shell-pty"
        ? ptyCapabilities(driver)
        : HUB_CAPABILITIES,
    ownerFence: "1",
    activeRunIds: [],
    parent: null,
    journalId: rec.journalId as Id,
    durableSeq: rec.durableSeq as U64,
    exit: { state: "not-applicable" },
    cwd: extra.cwd ?? rec.workspaceId ?? null,
    name: extra.name ?? rec.title ?? null,
    delegation: typeof rec.delegation === "string" ? rec.delegation : extra.delegation ?? null,
    providerProfileId:
      typeof rec.providerProfileId === "string" ? rec.providerProfileId : extra.providerProfileId ?? null,
    providerSource:
      typeof rec.providerSource === "string" ? rec.providerSource : extra.providerSource ?? null,
    providerSourceHint:
      typeof rec.providerSourceHint === "string" ? rec.providerSourceHint : extra.providerSourceHint ?? null,
    model: typeof extra.model === "string" ? extra.model : null,
    effortName: typeof extra.effortName === "string" ? extra.effortName : null,
    effortIndex: typeof extra.effortIndex === "number" ? extra.effortIndex : null,
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
  driver: "claude-print" | "claude-pty" | "claude-bg" | "generic-pty" | "shell-pty";
  model: string;
  providerProfileId?: string;
  permissionMode: string;
  delegation?: "none" | "gateway";
  prompt: string;
  worktree?: boolean | string;
  cwd?: string;
  settingsOverlayPath?: string;
  claudeConfigDir?: string;
  maxBudgetUsd?: string;
  name?: string;
  /** Composer/New Session effort. Stored in UI state; Hub ignores unknown create fields. */
  effortIndex?: number;
  effortName?: string;
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
  name: string;
  base?: string;
};

export type PtyKey = "enter" | "esc" | "ctrl+c";

/** Hub GET `/v1/providers` row. Auth token is never present. */
export type HubProviderRow = {
  id: string;
  name: string;
  kind: "gateway" | "direct" | string;
  baseUrl: string;
  models: string[];
  defaultModel?: string | null;
  headers?: Record<string, string>;
  defaultGateway: boolean;
  scope?: string;
  revision: string;
  secret: { present: boolean; last4?: string | null; fingerprint?: string | null };
  health?: { ok: boolean; checkedAt?: string | null; message?: string | null; status?: number | null; latencyMs?: number | null } | null;
  createdAt?: string;
  updatedAt?: string;
};

/** `POST /v1/fleet/broadcast` request body. */
export type FleetBroadcastBody = HubBody<"/v1/fleet/broadcast", "post">;
/** `POST /v1/fleet/broadcast` 200 body. */
export type FleetBroadcastResult = HubJson<"/v1/fleet/broadcast", "post">;
/** One instance's outcome inside a broadcast. */
export type FleetBroadcastEntry = NonNullable<FleetBroadcastResult["results"]>[number];

export type HubApi = {
  mock: boolean;
  login(bootstrapToken: string, deviceName: string): Promise<DeviceSession>;
  pairRedeem(code: string, deviceName: string): Promise<DeviceSession>;
  pairCode(): Promise<PairCode>;
  deviceList(): Promise<{ items: PairedDevice[] }>;
  deviceRevoke(deviceId: string): Promise<{ ok: boolean }>;
  hasDeviceSession(): boolean;
  hello(): Promise<HelloResult>;
  instanceList(q?: { hostId?: string; workspaceId?: string; kind?: string }): Promise<Page<Instance>>;
  instanceGet(instanceId: Id): Promise<Instance>;
  instanceCreate(spec: InstanceCreateSpec): Promise<{ command: CommandResult["command"]; instance: Instance }>;
  instanceSend(instanceId: Id, prompt: string): Promise<CommandResult>;
  instanceKeys(instanceId: Id, key: PtyKey): Promise<CommandResult>;
  fleetBroadcast(body: FleetBroadcastBody): Promise<FleetBroadcastResult>;
  worktreeList(hostId?: string): Promise<WorktreePage>;
  worktreeCreate(spec: WorktreeCreateSpec): Promise<WorktreeRecord>;
  screenRead(instanceId: Id, lines?: number): Promise<ScreenRead>;
  instanceClose(instanceId: Id): Promise<CommandResult>;
  instanceResume(instanceId: Id): Promise<CommandResult>;
  instanceConfigure(instanceId: Id, permissionMode: string, extras?: InstanceConfigurePatch): Promise<CommandResult>;
  interactionList(q?: { instanceId?: Id; state?: string }): Promise<Interaction[]>;
  interactionGet(interactionId: Id): Promise<Interaction>;
  interactionRespond(interactionId: Id, answer: InteractionAnswer): Promise<CommandResult>;
  hostSshAdd(body: components["schemas"]["SshHostCreate"]): Promise<Host>;
  hostRemove(hostId: Id): Promise<void>;
  hostList(): Promise<Page<Host>>;
  hostGet(hostId: Id): Promise<Host>;
  hostPatch(hostId: Id, body: { name?: string; labels?: string[]; maxInstances?: number; providerBinding?: string }): Promise<Host>;
  workspaceList(hostId?: Id): Promise<Page<Workspace>>;
  providerList(q?: { hostId?: string }): Promise<{ items: HubProviderRow[]; nextCursor?: string | null }>;
  providerGet(id: string): Promise<HubProviderRow>;
  providerCreate(body: ProviderCreate): Promise<HubProviderRow>;
  providerPatch(id: string, body: ProviderPatch): Promise<HubProviderRow>;
  providerDelete(id: string): Promise<{ ok: boolean }>;
  providerTest(id: string): Promise<ProviderTestResult>;
  eventsRead: JournalRead;
  eventsSubscribe(
    journalId: Id,
    afterSeq: U64 | null,
    onBatch: (batch: EventsBatch["params"]) => void,
  ): Promise<{
    subscriptionId: Id;
    journalId: Id;
    snapshot: Snapshot;
    floorSeq: U64;
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

async function rest<T>(path: string, req: RequestInit = {}): Promise<T> {
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
    try {
      const body = JSON.parse(text) as { code?: string; error?: string; reasons?: unknown };
      if (body.code) code = body.code;
      if (body.error) message = body.error;
      if (Array.isArray(body.reasons)) {
        reasons = body.reasons.filter((reason): reason is string => typeof reason === "string" && reason.length > 0);
        if (reasons.length > 0) {
          message = [message, ...reasons].join(" · ");
        }
      }
    } catch {
      /* raw */
    }
    throw new HubHttpError(res.status, code, message, reasons);
  }
  if (res.status === 204) return undefined as T;
  return (await res.json()) as T;
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
    async instanceSend(instanceId, prompt) {
      return mockSend(instanceId, prompt);
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
    async instanceResume(instanceId) {
      return mockResume(instanceId);
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
      return mockPage(mockDb.hosts);
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
        defaultModel: body.defaultModel ?? body.models[0] ?? null,
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
        : { ok: true, reachable: true, status: 200, latencyMs: 12, message: "reachable (200); 1 models", models: found.models };
      found.health = dummy
        ? { ok: false, message: result.message }
        : { ok: true, status: 200, latencyMs: 12, message: result.message, checkedAt: now() };
      return result;
    },
    async workspaceList(hostId) {
      const items = hostId ? mockDb.workspaces.filter((w) => w.hostId === hostId) : mockDb.workspaces;
      return mockPage(items);
    },
    eventsRead: async ({ journalId, afterSeq, limit }) => mockReadJournal(journalId, afterSeq, limit),
    async eventsSubscribe(journalId, _afterSeq, onBatch) {
      const instance = mockDb.instances.find((i) => i.journalId === journalId);
      if (!instance) throw new Error("JOURNAL_NOT_FOUND");
      const subscriptionId = id("sub_");
      subs.set(subscriptionId, onBatch);
      const snapshot = mockSnapshot(instance);
      let page: { events: Observation[]; durableSeq: string };
      try {
        page = mockReadJournal(journalId, snapshot.asOfSeq, 128);
      } catch {
        page = { events: [], durableSeq: snapshot.asOfSeq };
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
        floorSeq: "1",
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
    async instanceSend(instanceId, prompt) {
      return command(instanceId, "instance.send", { prompt });
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
      } catch {
        return { lines: [] };
      }
    },
    async instanceClose(instanceId) {
      return command(instanceId, "instance.close", {});
    },
    async instanceResume(instanceId) {
      return command(instanceId, "instance.resume", {});
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
    async workspaceList(hostId) {
      const listed = hostId ? [await this.hostGet(hostId)] : (await this.hostList()).items;
      const trees = await this.worktreeList(hostId).catch(() => ({ items: [] as WorktreeRecord[], workspaceRoot: null as string | null }));
      const fromHosts: Workspace[] = listed.map((h) => ({
        id: h.id,
        revision: "1",
        createdAt: h.createdAt,
        updatedAt: h.updatedAt,
        hostId: h.id,
        label: h.label,
        rootPath: h.ssh?.workspaceRoot || trees.workspaceRoot || "/",
        writePolicy: "default",
        canonicalRoot: h.ssh?.workspaceRoot ? known(h.ssh.workspaceRoot) : trees.workspaceRoot ? known(trees.workspaceRoot) : unknownKnowledge("none"),
      }));
      const fromTrees: Workspace[] = trees.items.map((row) => ({
        id: row.path as Id,
        revision: "1",
        createdAt: "",
        updatedAt: "",
        hostId: (row.hostId ?? listed[0]?.id ?? "") as Id,
        label: row.name,
        rootPath: row.path,
        writePolicy: "isolated-worktree",
        canonicalRoot: known(row.path),
        worktreeLabel: row.name,
        branch: row.branch,
      }));
      const items = [...fromTrees, ...fromHosts.filter((h) => !fromTrees.some((t) => t.hostId === h.hostId && t.rootPath === h.rootPath))];
      for (const w of items) workspaces.set(w.id, w);
      return { items, nextCursor: null };
    },
    eventsRead: async (args) => {
      const instanceId = instanceIdOf(args.journalId);
      const qs = args.afterSeq ? `?afterSeq=${encodeURIComponent(args.afterSeq)}` : "";
      const page = await rest<HubJson<"/v1/instances/{id}/journal", "get">>(
        `/v1/instances/${instanceId}/journal${qs}`,
      );
      const events = coerceObservationList(page.events, args.journalId, instanceId).slice(0, args.limit);
      return {
        events,
        durableSeq: page.durableSeq as U64,
        floorSeq: "1" as U64,
      };
    },
    async eventsSubscribe(journalId, afterSeq, onBatch) {
      const instanceId = instanceIdOf(journalId);
      follows.get(journalId)?.close();
      const subscriptionId = id("sub_") as Id;
      const ws = new WebSocket(followUrl(instanceId));
      follows.set(journalId, ws);
      followSubs.set(subscriptionId, journalId);
      const asOfSeq = await new Promise<U64>((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error("FOLLOW_SNAPSHOT_TIMEOUT")), 10_000);
        let settled = false;
        const finish = (seq: U64) => {
          if (settled) return;
          settled = true;
          clearTimeout(timer);
          resolve(seq);
        };
        ws.addEventListener("error", () => {
          if (settled) return;
          settled = true;
          clearTimeout(timer);
          reject(new Error("FOLLOW_CONNECT_FAILED"));
        });
        ws.addEventListener("message", (ev) => {
          if (typeof ev.data !== "string") return;
          let msg: { type?: string; asOfSeq?: string; seq?: string; events?: unknown; event?: unknown };
          try {
            msg = JSON.parse(ev.data) as typeof msg;
          } catch {
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
            finish((msg.asOfSeq ?? afterSeq ?? "0") as U64);
            return;
          }
          if (msg.type === "event") {
            const obs = coerceObservation(msg.event ?? msg, journalId, instanceId, msg.seq);
            if (!obs) return;
            onBatch({
              subscriptionId,
              journalId,
              fromSeq: obs.seq,
              toSeq: obs.seq,
              events: [obs],
              durableSeq: obs.seq,
            });
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
          history: { earliestRetainedSeq: "1", complete: true },
        },
        floorSeq: "1" as U64,
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

export const api: HubApi = MOCK ? createMockApi() : createLiveApi();

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
