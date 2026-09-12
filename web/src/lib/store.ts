import { useSyncExternalStore } from "react";
import type { Host, Instance } from "../types/instance";
import type { Interaction, InteractionAnswer } from "../types/interaction";
import type { Observation } from "../types/observation";
import type { Id } from "../types/wire";
import type { Workspace } from "../types/workspace";
import { api, type InstanceCreateSpec } from "./api";
import { JournalClient } from "./journal";

export type ConnectionUi = "live" | "reconnecting" | "offline";
export type Toast = { id: string; text: string } | null;

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
    const history = await api.eventsRead({ journalId: instance.journalId, limit: 512 });
    this.emit({
      events: { ...this.state.events, [instanceId]: history.events },
    });
    const client = new JournalClient(instance.journalId, api.eventsRead, {
      onEvents: (events) => {
        const current = this.state.events[instanceId] ?? [];
        this.emit({ events: { ...this.state.events, [instanceId]: current.concat(events) } });
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
    await api.instanceSend(instanceId, prompt);
    await this.catchup(instanceId);
  }

  async close(instanceId: Id) {
    await api.instanceClose(instanceId);
    await this.refresh();
  }

  async respond(interactionId: Id, answer: InteractionAnswer) {
    await api.interactionRespond(interactionId, answer);
    await this.refresh();
    const interaction = this.state.interactions.find((i) => i.id === interactionId);
    if (interaction) await this.catchup(interaction.instanceId);
  }

  titleOf(instanceId: Id) {
    return api.titleOf(instanceId);
  }

  hostName(hostId: Id) {
    return this.state.hosts.find((h) => h.id === hostId)?.label ?? api.hostName(hostId);
  }

  workspaceOf(workspaceId: Id): Workspace | undefined {
    return this.state.workspaces.find((w) => w.id === workspaceId);
  }
}

export const hubStore = new HubStore();

export function useHub(): HubState {
  return useSyncExternalStore(hubStore.subscribe, hubStore.getSnapshot, hubStore.getSnapshot);
}
