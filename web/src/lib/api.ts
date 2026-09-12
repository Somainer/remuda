import type { CommandResult, Page } from "../types/command";
import type { Host, Instance } from "../types/instance";
import type { Interaction, InteractionAnswer } from "../types/interaction";
import type { EventsBatch, Observation, Snapshot } from "../types/observation";
import type { Id, U64 } from "../types/wire";
import type { Workspace } from "../types/workspace";
import type { JournalRead } from "./journal";
import {
  mockClose,
  mockConfigure,
  mockCreate,
  mockDb,
  mockHostName,
  mockPage,
  mockReadJournal,
  mockRespond,
  mockResume,
  mockSend,
  mockSnapshot,
  mockWorkspaceLabel,
} from "./mock";
import { digestPlaceholder, id, now } from "./ids";
import { accessHeaders } from "./accessCode";

export const MOCK = import.meta.env.VITE_MOCK === "1";

export type HelloResult = {
  protocol: { major: number; minor: number };
  connectionId: Id;
  serverEpoch: Id;
  observationSchemaMajor: 1;
  features: string[];
};

export type InstanceCreateSpec = {
  hostId: Id;
  workspaceId: Id;
  kind: "claude" | "codex" | "grok" | "agy";
  driver: "claude-print" | "claude-pty" | "claude-bg";
  model: string;
  providerProfileId: string;
  permissionMode: string;
  prompt: string;
  worktree?: boolean;
  settingsOverlayPath?: string;
  claudeConfigDir?: string;
  maxBudgetUsd?: string;
  name?: string;
};

export type RpcError = { code: number; message: string; data?: unknown };

type JsonRpcSuccess<T> = { jsonrpc: "2.0"; id: string; result: T };
type JsonRpcFailure = { jsonrpc: "2.0"; id: string; error: RpcError };
type JsonRpcResponse<T> = JsonRpcSuccess<T> | JsonRpcFailure;

export type HubApi = {
  mock: boolean;
  hello(): Promise<HelloResult>;
  instanceList(q?: { hostId?: string; workspaceId?: string; kind?: string }): Promise<Page<Instance>>;
  instanceGet(instanceId: Id): Promise<Instance>;
  instanceCreate(spec: InstanceCreateSpec): Promise<{ command: CommandResult["command"]; instance: Instance }>;
  instanceSend(instanceId: Id, prompt: string): Promise<CommandResult>;
  instanceClose(instanceId: Id): Promise<CommandResult>;
  instanceResume(instanceId: Id): Promise<CommandResult>;
  instanceConfigure(instanceId: Id, permissionMode: string): Promise<CommandResult>;
  interactionList(q?: { instanceId?: Id; state?: string }): Promise<Interaction[]>;
  interactionGet(interactionId: Id): Promise<Interaction>;
  interactionRespond(interactionId: Id, answer: InteractionAnswer): Promise<CommandResult>;
  hostList(): Promise<Page<Host>>;
  hostGet(hostId: Id): Promise<Host>;
  workspaceList(hostId?: Id): Promise<Page<Workspace>>;
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
  const raw = import.meta.env.VITE_API_BASE ?? import.meta.env.VITE_HUB_URL ?? "";
  return raw.replace(/\/$/, "");
}

function wsUrl(): string {
  const base = hubBase();
  if (base.startsWith("https://")) return `${base.replace(/^https/, "wss")}/v1/client`;
  if (base.startsWith("http://")) return `${base.replace(/^http/, "ws")}/v1/client`;
  const proto = location.protocol === "https:" ? "wss" : "ws";
  return `${proto}://${location.host}/v1/client`;
}

async function rest<T>(path: string, init: RequestInit = {}): Promise<T> {
  const res = await fetch(`${hubBase()}${path}`, {
    credentials: "include",
    ...init,
    headers: { ...accessHeaders(), ...(init.headers as Record<string, string> | undefined) },
  });
  if (!res.ok) {
    const text = await res.text();
    throw new Error(text || `HTTP ${res.status}`);
  }
  if (res.status === 204) return undefined as T;
  return (await res.json()) as T;
}

async function postRpc<T>(method: string, params: unknown): Promise<T> {
  const rpcId = crypto.randomUUID();
  const body = await rest<JsonRpcResponse<T>>("/v1/rpc", {
    method: "POST",
    body: JSON.stringify({ jsonrpc: "2.0", id: rpcId, method, params }),
  });
  if ("error" in body) throw new Error(body.error.message);
  return body.result;
}

function createMockApi(): HubApi {
  const subs = new Map<Id, (batch: EventsBatch["params"]) => void>();
  return {
    mock: true,
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
      const instance = mockCreate(spec.prompt, {
        hostId: spec.hostId,
        workspaceId: spec.workspaceId,
        driver: spec.driver,
        kind: spec.kind,
      });
      instance.hostId = spec.hostId;
      instance.workspaceId = spec.workspaceId;
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
    async instanceClose(instanceId) {
      return mockClose(instanceId);
    },
    async instanceResume(instanceId) {
      return mockResume(instanceId);
    },
    async instanceConfigure(instanceId, permissionMode) {
      return mockConfigure(instanceId, permissionMode);
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
    async hostList() {
      return mockPage(mockDb.hosts);
    },
    async hostGet(hostId) {
      const found = mockDb.hosts.find((h) => h.id === hostId);
      if (!found) throw new Error("HOST_NOT_FOUND");
      return found;
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
      const page = mockReadJournal(journalId, snapshot.asOfSeq, 128);
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
  let socket: WebSocket | null = null;
  const pending = new Map<string, { resolve: (v: unknown) => void; reject: (e: Error) => void }>();
  const batchHandlers = new Map<Id, (batch: EventsBatch["params"]) => void>();
  let rpcSeq = 0;
  const titles = new Map<Id, string>();
  const hosts = new Map<Id, Host>();
  const workspaces = new Map<Id, Workspace>();

  function send<T>(method: string, params: unknown): Promise<T> {
    if (socket && socket.readyState === WebSocket.OPEN) {
      const rpcId = `c${++rpcSeq}`;
      return new Promise<T>((resolve, reject) => {
        pending.set(rpcId, { resolve: (v) => resolve(v as T), reject });
        socket!.send(JSON.stringify({ jsonrpc: "2.0", id: rpcId, method, params }));
      });
    }
    return postRpc<T>(method, params);
  }

  function ensureSocket(): Promise<void> {
    if (socket && socket.readyState === WebSocket.OPEN) return Promise.resolve();
    return new Promise((resolve, reject) => {
      const ws = new WebSocket(wsUrl());
      ws.binaryType = "arraybuffer";
      socket = ws;
      ws.addEventListener("open", () => resolve());
      ws.addEventListener("error", () => reject(new Error("WSS_CONNECT_FAILED")));
      ws.addEventListener("message", (ev) => {
        if (typeof ev.data !== "string") {
          // tty-binary-v1 / object chunks on the same /v1/client socket; tty/ owns decode.
          return;
        }
        const msg = JSON.parse(ev.data) as JsonRpcResponse<unknown> & { method?: string; params?: EventsBatch["params"] };
        if (msg.method === "events.batch" && msg.params) {
          const handler = batchHandlers.get(msg.params.subscriptionId);
          handler?.(msg.params);
          return;
        }
        if (!("id" in msg) || msg.id == null) return;
        const waiter = pending.get(String(msg.id));
        if (!waiter) return;
        pending.delete(String(msg.id));
        if ("error" in msg) waiter.reject(new Error(msg.error.message));
        else waiter.resolve(msg.result);
      });
      ws.addEventListener("close", () => {
        for (const waiter of pending.values()) waiter.reject(new Error("WSS_CLOSED"));
        pending.clear();
      });
    });
  }

  async function command(instanceId: Id, body: Record<string, unknown>): Promise<CommandResult> {
    try {
      return await rest<CommandResult>(`/v1/instances/${instanceId}/commands`, {
        method: "POST",
        body: JSON.stringify(body),
      });
    } catch {
      // TODO(M0-11): remuda-node HTTP router may still be landing; JSON-RPC per protocol.md.
      const operation = String(body.operation ?? "instance.send");
      return send<CommandResult>(
        operation === "send" ? "instance.send" : operation === "close" ? "instance.close" : "interaction.respond",
        body,
      );
    }
  }

  return {
    mock: false,
    async hello() {
      await ensureSocket();
      return send<HelloResult>("runtime.hello", {
        protocol: { major: 1, minMinor: 0, maxMinor: 0 },
        observationSchemaMajors: [1],
        features: ["snapshot-follow-v1"],
      });
    },
    async instanceList(q) {
      try {
        return await rest<Page<Instance>>(`/v1/instances${q?.hostId ? `?hostId=${q.hostId}` : ""}`);
      } catch {
        return send<Page<Instance>>("instance.list", q ?? {});
      }
    },
    async instanceGet(instanceId) {
      try {
        return await rest<Instance>(`/v1/instances/${instanceId}`);
      } catch {
        return send<Instance>("instance.get", { instanceId });
      }
    },
    async instanceCreate(spec) {
      try {
        const created = await rest<{ command: CommandResult["command"]; instance: Instance }>("/v1/instances", {
          method: "POST",
          body: JSON.stringify({
            hostId: spec.hostId,
            workspaceId: spec.workspaceId,
            kind: spec.kind,
            driver: spec.driver,
            model: spec.model,
            providerProfileId: spec.providerProfileId,
            permissionMode: spec.permissionMode,
            prompt: spec.prompt,
            settingsOverlayPath: spec.settingsOverlayPath,
            claudeConfigDir: spec.claudeConfigDir,
            maxBudgetUsd: spec.maxBudgetUsd,
            name: spec.name,
            worktree: spec.worktree,
          }),
        });
        titles.set(created.instance.id, spec.prompt.slice(0, 80) || spec.name || "会话");
        return created;
      } catch {
        // TODO(M0-11): remuda-node HTTP router may still be landing; JSON-RPC per protocol.md.
        return send("instance.create", { spec, initialInput: { type: "prompt", text: spec.prompt } });
      }
    },
    async instanceSend(instanceId, prompt) {
      return command(instanceId, { operation: "send", prompt });
    },
    async instanceClose(instanceId) {
      return command(instanceId, { operation: "close" });
    },
    async instanceResume(instanceId) {
      try {
        return await command(instanceId, { operation: "resume" });
      } catch {
        return send<CommandResult>("instance.resume", { instanceId });
      }
    },
    async instanceConfigure(instanceId, permissionMode) {
      return send<CommandResult>("instance.configure", { instanceId, permissionMode, effective: "next-turn" });
    },
    async interactionList(q) {
      try {
        const page = await rest<{ items?: Interaction[] } | Interaction[]>("/v1/interactions");
        const items = Array.isArray(page) ? page : (page.items ?? []);
        return items.filter((i) => {
          if (q?.instanceId && i.instanceId !== q.instanceId) return false;
          if (q?.state && i.state !== q.state) return false;
          return true;
        });
      } catch {
        const page = await send<{ items?: Interaction[] } | Interaction[]>("interaction.list", q ?? {});
        return Array.isArray(page) ? page : (page.items ?? []);
      }
    },
    async interactionGet(interactionId) {
      try {
        return await rest<Interaction>(`/v1/interactions/${interactionId}`);
      } catch {
        return send<Interaction>("interaction.get", { interactionId });
      }
    },
    async interactionRespond(interactionId, answer) {
      const found = await (async () => {
        try {
          return await rest<Interaction>(`/v1/interactions/${interactionId}`);
        } catch {
          return send<Interaction>("interaction.get", { interactionId });
        }
      })();
      return command(found.instanceId, {
        operation: "respond_interaction",
        interactionId,
        answer,
      });
    },
    async hostList() {
      try {
        const page = await rest<Page<Host>>("/v1/hosts");
        for (const h of page.items) hosts.set(h.id, h);
        return page;
      } catch {
        const page = await send<Page<Host>>("host.list", {});
        for (const h of page.items) hosts.set(h.id, h);
        return page;
      }
    },
    async hostGet(hostId) {
      const host = await send<Host>("host.get", { hostId });
      hosts.set(host.id, host);
      return host;
    },
    async workspaceList(hostId) {
      try {
        const page = await rest<Page<Workspace>>(hostId ? `/v1/workspaces?hostId=${hostId}` : "/v1/workspaces");
        for (const w of page.items) workspaces.set(w.id, w);
        return page;
      } catch {
        const page = await send<Page<Workspace>>("workspace.list", { hostId });
        for (const w of page.items) workspaces.set(w.id, w);
        return page;
      }
    },
    eventsRead: (args) => send("events.read", args),
    async eventsSubscribe(journalId, afterSeq, onBatch) {
      await ensureSocket();
      const result = await send<{
        subscriptionId: Id;
        journalId: Id;
        snapshot: Snapshot;
        floorSeq: U64;
        durableSeq: U64;
      }>("events.subscribe", {
        journalId,
        afterSeq,
        snapshot: "required",
        projectionVersion: "v1",
        batchLimit: 128,
      });
      batchHandlers.set(result.subscriptionId, onBatch);
      return result;
    },
    async eventsAck(subscriptionId, journalId, throughSeq) {
      return send("events.ack", { subscriptionId, journalId, throughSeq });
    },
    async eventsUnsubscribe(subscriptionId) {
      batchHandlers.delete(subscriptionId);
      await send("events.unsubscribe", { subscriptionId });
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
      socket?.close();
      socket = null;
    },
  };
}

export const api: HubApi = MOCK ? createMockApi() : createLiveApi();

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
