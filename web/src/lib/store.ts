import { useSyncExternalStore } from "react";
import type { Command } from "../types/command";
import type { Host, Instance } from "../types/instance";
import type { Interaction, InteractionAnswer } from "../types/interaction";
import type { Observation } from "../types/observation";
import type { Id } from "../types/wire";
import type { Workspace } from "../types/workspace";
import { api, observationText, type InstanceCreateSpec, type PtyKey, type WorktreeCreateSpec } from "./api";
import { doneFromLines, lastLines, latestScreenFromObservations } from "./screen";
import { isUnauthorized } from "./httpError";
import { JournalClient, type JournalRead } from "./journal";
import { id, now } from "./ids";
import { mockGappedTail, mockJournalIds } from "./mock";
import { readDeviceSettings } from "../features/settings/prefs";
import {
  MOCK_BOOTSTRAP_TOKEN,
  clearSession,
  dropDeviceCookie,
  readLoggedOut,
  readSession,
  writeSession,
  type DeviceSession,
  type PairCode,
  type PairedDevice,
} from "./session";

export type ConnectionUi = "live" | "reconnecting" | "offline";
export type Toast = { id: string; text: string } | null;
export type LocalBubble = {
  id: Id;
  instanceId: Id;
  text: string;
  commandId: Id;
  state: Command["state"] | "unknown";
  createdAt: string;
};

const COMPACT_KEY = "runtime.compact";

export type HubState = {
  ready: boolean;
  authed: boolean;
  error: string | null;
  toast: Toast;
  connection: ConnectionUi;
  session: DeviceSession | null;
  devices: PairedDevice[];
  pairCode: PairCode | null;
  instances: Instance[];
  hosts: Host[];
  workspaces: Workspace[];
  interactions: Interaction[];
  events: Record<string, Observation[]>;
  journalStatus: Record<string, JournalClient["status"]>;
  bubbles: LocalBubble[];
  permissionMode: Record<string, string>;
  compact: boolean;
  answering: Record<string, true>;
  screens: Record<string, { lines: string[]; done: boolean }>;
};

const initial: HubState = {
  ready: false,
  authed: false,
  error: null,
  toast: null,
  connection: "live",
  session: readSession(),
  devices: [],
  pairCode: null,
  instances: [],
  hosts: [],
  workspaces: [],
  interactions: [],
  events: {},
  journalStatus: {},
  bubbles: [],
  permissionMode: {},
  compact: typeof localStorage === "undefined" ? true : localStorage.getItem(COMPACT_KEY) !== "0",
  answering: {},
  screens: {},
};

type Listener = () => void;

class HubStore {
  private state: HubState = initial;
  private listeners = new Set<Listener>();
  private journals = new Map<Id, JournalClient>();
  private subs = new Map<Id, Id>();
  private bootGen = 0;
  private pollTimer: ReturnType<typeof setInterval> | null = null;

  subscribe = (listener: Listener) => {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  };

  getSnapshot = () => this.state;

  private emit(patch: Partial<HubState>) {
    this.state = { ...this.state, ...patch };
    for (const listener of this.listeners) listener();
  }

  toast(text: string) {
    this.emit({ toast: { id: String(Date.now()), text } });
  }

  clearToast() {
    this.emit({ toast: null });
  }

  setAuthed(authed: boolean) {
    this.emit({ authed });
  }

  setCompact(compact: boolean) {
    try {
      localStorage.setItem(COMPACT_KEY, compact ? "1" : "0");
    } catch {
      /* ignore */
    }
    this.emit({ compact });
  }

  async bootstrap() {
    const gen = ++this.bootGen;
    if (api.mock && readLoggedOut() && !readSession()) {
      this.emit({ ready: true, authed: false, session: null, devices: [], connection: "offline" });
      return;
    }
    if (api.mock && !readSession()) {
      const session = await api.login(MOCK_BOOTSTRAP_TOKEN, readDeviceSettings().deviceName);
      writeSession(session);
      this.emit({ session });
    }
    try {
      await api.hello();
      if (gen !== this.bootGen) return;
      if (!api.mock && !api.hasDeviceSession()) {
        this.emit({ ready: true, authed: false, error: null, connection: "live" });
        return;
      }
      const [instances, hosts, workspaces, interactions, devices] = await Promise.all([
        api.instanceList(),
        api.hostList(),
        api.workspaceList(),
        api.interactionList(),
        api.deviceList().catch(() => ({ items: [] as PairedDevice[] })),
      ]);
      if (gen !== this.bootGen) return;
      this.emit({
        ready: true,
        authed: true,
        error: null,
        connection: "live",
        session: readSession(),
        devices: devices.items,
        instances: instances.items,
        hosts: hosts.items,
        workspaces: workspaces.items,
        interactions,
      });
      this.startPoll();
    } catch (err) {
      if (gen !== this.bootGen) return;
      const unauth = isUnauthorized(err);
      if (unauth) {
        clearSession();
        dropDeviceCookie();
      }
      this.emit({
        ready: true,
        authed: false,
        session: unauth ? null : this.state.session,
        error: err instanceof Error ? err.message : "bootstrap failed",
        connection: "offline",
      });
    }
  }

  async login(kind: "bootstrap" | "pair", secret: string, deviceName: string) {
    const session =
      kind === "pair" ? await api.pairRedeem(secret, deviceName) : await api.login(secret, deviceName);
    writeSession(session);
    this.emit({ session, authed: true, error: null });
    await this.bootstrap();
  }

  startPoll() {
    if (this.pollTimer != null || typeof window === "undefined") return;
    this.pollTimer = window.setInterval(() => {
      if (!this.state.authed) return;
      void this.refresh();
    }, 2000);
  }

  logout() {
    const mine = this.state.session?.deviceId;
    if (mine) void api.deviceRevoke(mine).catch(() => undefined);
    clearSession();
    dropDeviceCookie();
    api.disconnect();
    this.emit({
      authed: false,
      session: null,
      devices: [],
      pairCode: null,
      instances: [],
      hosts: [],
      workspaces: [],
      interactions: [],
      events: {},
      connection: "offline",
    });
  }

  async issuePairCode() {
    const issued = await api.pairCode();
    this.emit({ pairCode: issued });
    return issued;
  }

  async refreshDevices() {
    const devices = await api.deviceList();
    this.emit({ devices: devices.items });
  }

  async revokeDevice(deviceId: string) {
    await api.deviceRevoke(deviceId);
    if (this.state.session?.deviceId === deviceId) {
      this.logout();
      return;
    }
    await this.refreshDevices();
  }

  async refresh() {
    const [instances, interactions] = await Promise.all([api.instanceList(), api.interactionList()]);
    this.emit({ instances: instances.items, interactions });
  }

  async follow(instanceId: Id) {
    const instance = this.state.instances.find((i) => i.id === instanceId) ?? (await api.instanceGet(instanceId));
    if (!this.state.instances.some((i) => i.id === instanceId)) {
      this.emit({ instances: [instance, ...this.state.instances] });
    }
    if (this.journals.has(instance.journalId)) return;
    this.emit({ journalStatus: { ...this.state.journalStatus, [instanceId]: "live" } });
    const history: Observation[] = [];
    let afterSeq: Observation["seq"] | undefined;
    for (;;) {
      const page = await api.eventsRead({ journalId: instance.journalId, afterSeq, limit: 512 });
      history.push(...page.events);
      if (page.events.length < 512) break;
      afterSeq = page.events[page.events.length - 1].seq;
      if (history.length >= 8192) break;
    }
    this.emit({
      events: { ...this.state.events, [instanceId]: history },
    });
    const read: JournalRead = async (args) => {
      if (args.journalId === mockJournalIds.journalGap && args.afterSeq && Number(args.afterSeq) > 0) {
        await new Promise((resolve) => setTimeout(resolve, 500));
      }
      return api.eventsRead(args);
    };
    const last = history.at(-1)?.seq ?? ("0" as Observation["seq"]);
    const client = new JournalClient(instance.journalId, read, {
      onEvents: (events) => {
        const current = this.state.events[instanceId] ?? [];
        const seen = new Set(current.map((e) => e.eventId));
        const next = current.concat(events.filter((e) => !seen.has(e.eventId)));
        const screen = latestScreenFromObservations(next);
        this.emit({
          events: { ...this.state.events, [instanceId]: next },
          bubbles: settleBubbles(this.state.bubbles, instanceId, next),
          screens: screen.lines.length
            ? {
                ...this.state.screens,
                [instanceId]: { lines: lastLines(screen.lines, 80), done: doneFromLines(screen.lines) },
              }
            : this.state.screens,
        });
      },
      onStatus: (status) => {
        this.emit({ journalStatus: { ...this.state.journalStatus, [instanceId]: status } });
      },
      onGap: (from, to) => {
        void client.fillGap(from, to).then((acked) => {
          if (acked) void api.eventsAck(this.subs.get(instance.journalId) ?? "resume", instance.journalId, acked);
        });
      },
    });
    this.journals.set(instance.journalId, client);
    client.applySnapshot({
      projectionVersion: "v1",
      projectionEpoch: id("epoch_"),
      asOfSeq: last,
      instance: {} as Instance,
      runs: [],
      commands: [],
      pendingInteractions: [],
      nodes: [],
      history: { earliestRetainedSeq: "1", complete: true },
    });
    const sub = await api.eventsSubscribe(instance.journalId, last, (batch) => {
      const result = client.applyBatch(batch);
      if (result.acked) void api.eventsAck(batch.subscriptionId, instance.journalId, result.acked);
      if (result.gap) void client.fillGap(result.gap.from, result.gap.to);
    });
    this.subs.set(instance.journalId, sub.subscriptionId);
    if (Number(sub.snapshot.asOfSeq) >= Number(last)) {
      client.applySnapshot(sub.snapshot);
    }
    const tail = mockGappedTail(instance.journalId);
    if (tail) {
      setTimeout(() => {
        const result = client.applyBatch({ ...tail, subscriptionId: sub.subscriptionId });
        if (result.gap) void client.fillGap(result.gap.from, result.gap.to);
      }, 0);
    }
    if (history.length === 0 && sub.snapshot.asOfSeq !== "0") {
      const page = await api.eventsRead({ journalId: instance.journalId, limit: 512 });
      this.emit({ events: { ...this.state.events, [instanceId]: page.events } });
    }
  }

  async catchup(instanceId: Id) {
    const instance = this.state.instances.find((i) => i.id === instanceId);
    if (!instance) return;
    const client = this.journals.get(instance.journalId);
    if (!client) {
      await this.follow(instanceId);
      return;
    }
    client.markReconnecting();
    this.emit({ connection: "reconnecting" });
    await client.resumeAfterReconnect();
    this.emit({ connection: "live" });
    await this.refresh();
  }

  async create(spec: InstanceCreateSpec) {
    const result = await api.instanceCreate(spec);
    this.emit({ instances: [result.instance, ...this.state.instances.filter((i) => i.id !== result.instance.id)] });
    await this.refresh();
    return this.state.instances.find((i) => i.id === result.instance.id) ?? result.instance;
  }

  async send(instanceId: Id, prompt: string) {
    const localId = id("local_");
    const bubble: LocalBubble = {
      id: localId,
      instanceId,
      text: prompt,
      commandId: localId,
      state: "queued",
      createdAt: now(),
    };
    this.emit({ bubbles: this.state.bubbles.concat(bubble) });
    try {
      const result = await api.instanceSend(instanceId, prompt);
      this.emit({
        bubbles: this.state.bubbles.map((b) =>
          b.id === localId ? { ...b, state: result.command.state, commandId: result.command.commandId } : b,
        ),
      });
      await this.catchup(instanceId);
      const events = this.state.events[instanceId] ?? [];
      this.emit({ bubbles: settleBubbles(this.state.bubbles, instanceId, events) });
      await this.refreshScreen(instanceId).catch(() => undefined);
    } catch {
      this.emit({
        bubbles: this.state.bubbles.map((b) => (b.id === localId ? { ...b, state: "unknown" } : b)),
      });
    }
  }

  retract(bubbleId: Id) {
    const bubble = this.state.bubbles.find((b) => b.id === bubbleId);
    if (!bubble || bubble.state === "accepted" || bubble.state === "settled") return;
    this.emit({ bubbles: this.state.bubbles.filter((b) => b.id !== bubbleId) });
  }

  async close(instanceId: Id) {
    await api.instanceClose(instanceId);
    await this.refresh();
  }

  async sendKeys(instanceId: Id, key: PtyKey) {
    await api.instanceKeys(instanceId, key);
    await this.refreshScreen(instanceId);
  }

  async createWorktree(spec: WorktreeCreateSpec) {
    const record = await api.worktreeCreate(spec);
    const [workspaces] = await Promise.all([api.workspaceList()]);
    this.emit({ workspaces: workspaces.items });
    return record;
  }

  async broadcast(instanceIds: Id[], prompt: string) {
    const text = prompt.trim();
    if (!text || !instanceIds.length) return;
    await Promise.all(instanceIds.map((id) => this.send(id, text)));
    this.toast(`已群发 ${instanceIds.length} 个实例`);
  }

  async refreshScreen(instanceId: Id) {
    let read = await api.screenRead(instanceId, 80);
    if (!read.lines.length) {
      read = latestScreenFromObservations(this.state.events[instanceId] ?? []);
    }
    const lines = lastLines(read.lines, 80);
    this.emit({
      screens: {
        ...this.state.screens,
        [instanceId]: { lines: lastLines(lines, 3), done: doneFromLines(read.lines) },
      },
    });
  }

  async refreshScreens(instanceIds: Id[]) {
    await Promise.all(instanceIds.map((id) => this.refreshScreen(id)));
  }

  async resume(instanceId: Id) {
    await api.instanceResume(instanceId);
    await this.refresh();
  }

  async configure(instanceId: Id, permissionMode: string) {
    await api.instanceConfigure(instanceId, permissionMode);
    this.emit({ permissionMode: { ...this.state.permissionMode, [instanceId]: permissionMode } });
  }

  async respond(interactionId: Id, answer: InteractionAnswer) {
    this.emit({ answering: { ...this.state.answering, [interactionId]: true } });
    try {
      await api.interactionRespond(interactionId, answer);
      await this.refresh();
      const interaction = this.state.interactions.find((i) => i.id === interactionId);
      if (interaction) await this.catchup(interaction.instanceId);
    } finally {
      const interaction = this.state.interactions.find((i) => i.id === interactionId);
      const events = interaction ? (this.state.events[interaction.instanceId] ?? []) : [];
      const answered = events.some(
        (ev) => ev.kind === "interaction.answered" && (ev.payload as { interactionId?: Id }).interactionId === interactionId,
      );
      if (!interaction || interaction.state !== "pending" || answered) {
        const { [interactionId]: _removed, ...rest } = this.state.answering;
        this.emit({ answering: rest });
      }
    }
  }

  titleOf(instanceId: Id) {
    return api.titleOf(instanceId);
  }

  summaryOf(instanceId: Id) {
    return api.summaryOf(instanceId);
  }

  permissionModeOf(instanceId: Id) {
    return this.state.permissionMode[instanceId] ?? api.permissionModeOf(instanceId);
  }

  hostName(hostId: Id) {
    return this.state.hosts.find((h) => h.id === hostId)?.label ?? api.hostName(hostId);
  }

  workspaceOf(workspaceId: Id): Workspace | undefined {
    return this.state.workspaces.find((w) => w.id === workspaceId);
  }
}

function settleBubbles(bubbles: LocalBubble[], instanceId: Id, events: Observation[]): LocalBubble[] {
  return bubbles.map((bubble) => {
    if (bubble.instanceId !== instanceId) return bubble;
    if (bubble.state === "queued") return bubble;
    const match = events.some(
      (ev) => ev.kind === "message" && observationText(ev) === bubble.text && (ev.payload as { role?: string }).role === "user",
    );
    return match ? { ...bubble, state: "settled" as const } : bubble;
  });
}

export const hubStore = new HubStore();

export function useHub(): HubState {
  return useSyncExternalStore(hubStore.subscribe, hubStore.getSnapshot, hubStore.getSnapshot);
}
