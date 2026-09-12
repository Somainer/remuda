import { useSyncExternalStore } from "react";
import type { Command } from "../types/command";
import type { Host, Instance } from "../types/instance";
import type { Interaction, InteractionAnswer } from "../types/interaction";
import type { Observation } from "../types/observation";
import type { Id } from "../types/wire";
import type { Workspace } from "../types/workspace";
import { api, observationText, type InstanceCreateSpec } from "./api";
import { JournalClient } from "./journal";
import { id, now } from "./ids";

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
};

const initial: HubState = {
  ready: false,
  authed: api.mock,
  error: null,
  toast: null,
  connection: "live",
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
};

type Listener = () => void;

class HubStore {
  private state: HubState = initial;
  private listeners = new Set<Listener>();
  private journals = new Map<Id, JournalClient>();
  private subs = new Map<Id, Id>();

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
    try {
      await api.hello();
      const [instances, hosts, workspaces, interactions] = await Promise.all([
        api.instanceList(),
        api.hostList(),
        api.workspaceList(),
        api.interactionList(),
      ]);
      this.emit({
        ready: true,
        authed: true,
        error: null,
        connection: "live",
        instances: instances.items,
        hosts: hosts.items,
        workspaces: workspaces.items,
        interactions,
      });
    } catch (err) {
      this.emit({
        ready: true,
        authed: api.mock,
        error: err instanceof Error ? err.message : "bootstrap failed",
        connection: "offline",
      });
    }
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
    const history = await api.eventsRead({ journalId: instance.journalId, limit: 512 });
    this.emit({
      events: { ...this.state.events, [instanceId]: history.events },
    });
    const client = new JournalClient(instance.journalId, api.eventsRead, {
      onEvents: (events) => {
        const current = this.state.events[instanceId] ?? [];
        const next = current.concat(events);
        this.emit({
          events: { ...this.state.events, [instanceId]: next },
          bubbles: settleBubbles(this.state.bubbles, instanceId, next),
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
    const sub = await api.eventsSubscribe(instance.journalId, client.appliedSeq, (batch) => {
      const result = client.applyBatch(batch);
      if (result.acked) void api.eventsAck(batch.subscriptionId, instance.journalId, result.acked);
      if (result.gap) void client.fillGap(result.gap.from, result.gap.to);
    });
    this.subs.set(instance.journalId, sub.subscriptionId);
    client.applySnapshot(sub.snapshot);
    if (history.events.length === 0 && sub.snapshot.asOfSeq !== "0") {
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
    return result.instance;
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
