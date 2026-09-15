import { useSyncExternalStore } from "react";
import type { Command } from "../types/command";
import type { Host, Instance } from "../types/instance";
import type { Interaction, InteractionAnswer } from "../types/interaction";
import type { Observation } from "../types/observation";
import type { Id } from "../types/wire";
import type { PromptMode } from "../types/generated";
import type { Workspace, WorkspaceSnapshot } from "../types/workspace";
import type { AttachmentRef } from "./attachments";
import { mapWorkspace, mergeHostWorkspaces } from "../features/workspaces/registry";
import {
  api,
  observationText,
  type InstanceCreateSpec,
  type PasskeyAssertionBody,
  type PasskeyAttestationBody,
  type PasskeyView,
  type PtyKey,
  type ResumeMode,
  type WorktreeCreateSpec,
} from "./api";
import {
  conditionalMediationAvailable,
  createPasskey,
  getPasskey,
  passkeysSupported,
  type ServerCreationOptions,
  type ServerRequestOptions,
} from "./passkeys";
import {
  DEFAULT_EFFORT_INDEX,
  effortAt,
  effortFromRecord,
  effortWireName,
  mapEffort,
  type EffortKind,
  type EffortSelection,
} from "../features/session/effort";
import {
  effectiveFromObservation,
  effectiveFromRecord,
  type EffortEffectiveView,
} from "../features/session/effortEffective";
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
  /**
   * Thumbnails for images sent with this message (D-027). Held locally
   * because the Hub does not echo attachments back onto the journal yet.
   */
  attachments?: BubbleAttachment[];
  /** D-028 §6 PromptMode used for this send; absent is a normal new turn. */
  promptMode?: PromptMode;
};

/**
 * One image shown under a sent bubble. `index` is its 1-based `[Image #n]`
 * anchor (from the send manifest), so an inline token can be paired with the
 * thumbnail.
 */
export type BubbleAttachment = {
  objectId: string;
  name: string;
  previewUrl: string;
  index?: number;
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
  passkeys: PasskeyView[];
  pairCode: PairCode | null;
  instances: Instance[];
  hosts: Host[];
  workspaces: Workspace[];
  interactions: Interaction[];
  events: Record<string, Observation[]>;
  journalStatus: Record<string, JournalClient["status"]>;
  bubbles: LocalBubble[];
  permissionMode: Record<string, string>;
  effort: Record<string, EffortSelection>;
  /** §9.1 transcript-read-back effective effort per instance; absent = `?`. */
  effortEffective: Record<string, EffortEffectiveView>;
  models: Record<string, string>;
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
  passkeys: [],
  pairCode: null,
  instances: [],
  hosts: [],
  workspaces: [],
  interactions: [],
  events: {},
  journalStatus: {},
  bubbles: [],
  permissionMode: {},
  effort: {},
  effortEffective: {},
  models: {},
  compact: typeof localStorage === "undefined" ? true : localStorage.getItem(COMPACT_KEY) !== "0",
  answering: {},
  screens: {},
};

type Listener = () => void;

/** Only Node-validated activity or its full Instance can change turn state. */
function applyInstanceActivity(instances: Instance[], events: Observation[]): Instance[] {
  return instances.map((instance) => {
    let current = instance;
    for (const event of events) {
      if (event.instanceId !== current.id || event.kind !== "lifecycle"
        || BigInt(event.seq) <= BigInt(current.durableSeq)) continue;
      if (event.payload.type === "native") {
        const activity = event.payload.relatedIds?.remudaActivity;
        if (current.driver !== "shell-pty" || event.payload.nativeName === "SubagentStop"
          || (activity !== "working" && activity !== "idle")) continue;
        current = {
          ...current,
          activity: { state: "known", value: activity },
          activityEvidenceEventIds: [event.eventId],
          updatedAt: event.observedAt,
          durableSeq: event.seq,
        };
        continue;
      }
      if (event.payload.type !== "entity" || event.payload.entityType !== "instance"
        || event.payload.entity.id !== current.id) continue;
      const entity = event.payload.entity;
      current = {
        ...current,
        activity: entity.activity,
        activityEvidenceEventIds: entity.activityEvidenceEventIds,
        nativeRef: { ...current.nativeRef, signalTier: entity.nativeRef.signalTier ?? undefined },
        updatedAt: entity.updatedAt,
        durableSeq: event.seq,
      };
    }
    return current;
  });
}

/** An HTTP poll started before a followed turn boundary cannot undo it. */
function mergeInstanceSnapshots(incoming: Instance[], current: Instance[]): Instance[] {
  const previous = new Map(current.map((instance) => [instance.id, instance]));
  return incoming.map((instance) => {
    const newer = previous.get(instance.id);
    return newer && BigInt(newer.durableSeq) > BigInt(instance.durableSeq) ? newer : instance;
  });
}

class HubStore {
  private state: HubState = initial;
  private listeners = new Set<Listener>();
  private journals = new Map<Id, JournalClient>();
  private subs = new Map<Id, Id>();
  private bootGen = 0;
  private pollTimer: ReturnType<typeof setInterval> | null = null;
  private stopWorkspaceFollow: (() => void) | null = null;

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

  /** §9.1: fold Hub-record `effortEffective` into the live map, newest wins. */
  private hydrateEffortEffective(instances: Instance[]) {
    let updated = false;
    const next = { ...this.state.effortEffective };
    for (const instance of instances) {
      const view = effectiveFromRecord(instance.effortEffective);
      if (!view) continue;
      const current = next[instance.id];
      if (!current || view.observedAt >= current.observedAt) {
        next[instance.id] = view;
        updated = true;
      }
    }
    if (updated) this.emit({ effortEffective: next });
  }

  /** Apply one transcript-read-back effort observation to the live map. */
  private noteEffortObservation(instanceId: Id, observation: Observation): boolean {
    const parsed = effectiveFromObservation(observation);
    if (!parsed) return false;
    const current = this.state.effortEffective[instanceId];
    if (current && parsed.effective.observedAt < current.observedAt) return false;
    this.emit({
      effortEffective: { ...this.state.effortEffective, [instanceId]: parsed.effective },
    });
    return true;
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
      this.emit({ ready: true, authed: false, session: null, devices: [], passkeys: [], connection: "offline" });
      return;
    }
    if (api.mock && !readSession()) {
      const session = await api.login(MOCK_BOOTSTRAP_TOKEN, readDeviceSettings().deviceName);
      writeSession(session, { mock: api.mock });
      this.emit({ session: readSession() });
    }
    try {
      await api.hello();
      if (gen !== this.bootGen) return;
      if (!api.mock && !api.hasDeviceSession()) {
        this.emit({ ready: true, authed: false, error: null, connection: "live" });
        return;
      }
      const [instances, hosts, interactions, devices, passkeys] = await Promise.all([
        api.instanceList(),
        api.hostList(),
        api.interactionList(),
        api.deviceList().catch(() => ({ items: [] as PairedDevice[] })),
        api.passkeyList().catch(() => ({ items: [] as PasskeyView[] })),
      ]);
      if (gen !== this.bootGen) return;
      const registeredHosts = mergeHostWorkspaces(hosts.items, this.state.hosts);
      this.emit({
        ready: true,
        authed: true,
        error: null,
        connection: "live",
        session: readSession(),
        devices: devices.items,
        passkeys: passkeys.items,
        instances: instances.items,
        hosts: registeredHosts,
        workspaces: registeredHosts.flatMap((host) => (host.workspaces ?? []).map(mapWorkspace)),
        interactions,
      });
      this.hydrateEffortEffective(instances.items);
      this.stopWorkspaceFollow?.();
      this.stopWorkspaceFollow = api.hostWorkspaceSubscribe(
        (snapshot) => this.applyWorkspaceSnapshot(snapshot),
        () => { void this.refreshHosts().catch(() => undefined); },
      );
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
    writeSession(session, { mock: api.mock });
    // bootstrap marks the session authenticated after cookie-backed reads finish.
    this.emit({ session: readSession(), error: null });
    await this.bootstrap();
  }

  /** Whether the browser exposes WebAuthn on this origin. */
  passkeysSupported(): boolean {
    return passkeysSupported();
  }

  stateAuthed(): boolean {
    return this.state.authed;
  }

  async conditionalMediationAvailable(): Promise<boolean> {
    return conditionalMediationAvailable();
  }

  /**
   * Passkey login. `conditional` keeps the ceremony pending until the user
   * picks an autofill suggestion; abort the provided signal when starting an
   * explicit (required) ceremony so the two do not overlap.
   */
  async passkeyLogin(
    mediation: "required" | "conditional",
    deviceName?: string,
    signal?: AbortSignal,
  ): Promise<{ assertion: PasskeyAssertionBody; challengeId: string } | void> {
    const envelope = await api.passkeyLoginStart(mediation === "conditional" ? "conditional" : undefined);
    const options = envelope.options as ServerRequestOptions;
    const assertion = await getPasskey(options, mediation, signal);
    const session = await api.passkeyLoginFinish(envelope.challengeId, assertion, deviceName);
    writeSession(session, { mock: api.mock });
    this.emit({ session: readSession(), error: null });
    await this.bootstrap();
    return { assertion, challengeId: envelope.challengeId };
  }

  /** Register a new passkey from settings (requires an authenticated device). */
  async addPasskey(name: string): Promise<PasskeyView> {
    const envelope = await api.passkeyRegisterStart(name);
    const options = envelope.options as ServerCreationOptions;
    const attestation: PasskeyAttestationBody = await createPasskey(options);
    const saved = await api.passkeyRegisterFinish(envelope.challengeId, attestation);
    await this.refreshPasskeys();
    return saved;
  }

  async refreshPasskeys() {
    const page = await api.passkeyList();
    this.emit({ passkeys: page.items });
  }

  async renamePasskey(passkeyId: string, name: string) {
    await api.passkeyRename(passkeyId, name);
    await this.refreshPasskeys();
  }

  async deletePasskey(passkeyId: string) {
    await api.passkeyDelete(passkeyId);
    await this.refreshPasskeys();
  }

  startPoll() {
    if (this.pollTimer != null || typeof window === "undefined") return;
    this.pollTimer = window.setInterval(() => {
      if (!this.state.authed) return;
      void this.refresh();
      void this.refreshHosts().catch(() => undefined);
    }, 2000);
  }

  logout() {
    const mine = this.state.session?.deviceId;
    if (mine) void api.deviceRevoke(mine).catch(() => undefined);
    clearSession();
    dropDeviceCookie();
    api.disconnect();
    this.stopWorkspaceFollow?.();
    this.stopWorkspaceFollow = null;
    this.emit({
      authed: false,
      session: null,
      devices: [],
      passkeys: [],
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

  async refreshHosts() {
    const page = await api.hostList();
    const hosts = mergeHostWorkspaces(page.items, this.state.hosts);
    this.emit({ hosts, workspaces: hosts.flatMap((host) => (host.workspaces ?? []).map(mapWorkspace)) });
  }

  private applyWorkspaceSnapshot(snapshot: WorkspaceSnapshot) {
    const host = this.state.hosts.find((row) => row.id === snapshot.hostId);
    if (!host || snapshot.workspaceRevision < (host.workspaceRevision ?? 0)) return;
    this.emit({
      hosts: this.state.hosts.map((row) => row.id === host.id
        ? { ...row, workspaceRevision: snapshot.workspaceRevision, workspaces: snapshot.workspaces } : row),
      workspaces: [...this.state.workspaces.filter((row) => row.hostId !== host.id), ...snapshot.workspaces.map(mapWorkspace)],
    });
  }

  async registerWorkspace(hostId: Id, path: string) {
    const page = await api.workspaceRegister(hostId, path);
    this.applyWorkspaceSnapshot({ hostId, workspaceRevision: page.workspaceRevision ?? 0,
      workspaces: page.items.map((w) => ({ workspaceId: w.id, hostId: w.hostId, root: w.rootPath })) });
    return page.items.find((w) => w.id === page.workspaceId || w.rootPath === path);
  }

  async unregisterWorkspace(hostId: Id, path: string) {
    const page = await api.workspaceUnregister(hostId, path);
    this.applyWorkspaceSnapshot({ hostId, workspaceRevision: page.workspaceRevision ?? 0,
      workspaces: page.items.map((w) => ({ workspaceId: w.id, hostId: w.hostId, root: w.rootPath })) });
  }

  async refresh() {
    const [instances, interactions] = await Promise.all([api.instanceList(), api.interactionList()]);
    this.emit({
      instances: mergeInstanceSnapshots(instances.items, this.state.instances),
      interactions,
    });
    this.hydrateEffortEffective(instances.items);
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
    // Another mount may finish loading this journal while this read is pending.
    if (this.journals.has(instance.journalId)) return;
    this.emit({
      instances: applyInstanceActivity(this.state.instances, history),
      events: { ...this.state.events, [instanceId]: history },
    });
    // §9.1: the Hub record usually already carries the latest effective level;
    // replay history effort edges too so a reconnect before refresh is honest.
    for (const event of history) this.noteEffortObservation(instanceId, event);
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
        const fresh = events.filter((e) => !seen.has(e.eventId));
        // §9.1: live effort edges update the effective level immediately —
        // the slider reflects the transcript, not the optimistic request.
        for (const event of fresh) this.noteEffortObservation(instanceId, event);
        const next = current.concat(fresh);
        const screen = latestScreenFromObservations(next);
        this.emit({
          instances: applyInstanceActivity(this.state.instances, events),
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
    const createdId = result.instance.id;
    const kind = spec.kind as EffortKind;
    const effort =
      spec.effortName != null
        ? effortFromRecord(kind, spec.effortName, spec.effortIndex) ??
          effortAt(kind, spec.effortIndex ?? DEFAULT_EFFORT_INDEX)
        : effortAt(kind, spec.effortIndex ?? DEFAULT_EFFORT_INDEX);
    this.emit({
      instances: [result.instance, ...this.state.instances.filter((i) => i.id !== createdId)],
      permissionMode: { ...this.state.permissionMode, [createdId]: spec.permissionMode },
      effort: { ...this.state.effort, [createdId]: effort },
      models: { ...this.state.models, [createdId]: spec.model },
    });
    await this.refresh();
    return this.state.instances.find((i) => i.id === result.instance.id) ?? result.instance;
  }

  async send(
    instanceId: Id,
    prompt: string,
    attachments: AttachmentRef[] = [],
    previews: BubbleAttachment[] = [],
    mode?: PromptMode,
  ) {
    // Anchor mapping (2026-09-15): the manifest carries the [Image #n] index
    // in token order; pair it onto the local bubble's previews so the token
    // renders as an inline thumbnail chip.
    const indexOf = new Map(attachments.map((ref) => [ref.objectId, ref.index]));
    const numberedPreviews = previews.map((preview) => {
      const index = indexOf.get(preview.objectId);
      return index ? { ...preview, index } : preview;
    });
    const localId = id("local_");
    const bubble: LocalBubble = {
      id: localId,
      instanceId,
      text: prompt,
      ...(numberedPreviews.length ? { attachments: numberedPreviews } : {}),
      commandId: localId,
      state: "queued",
      ...(mode ? { promptMode: mode } : {}),
      createdAt: now(),
    };
    this.emit({ bubbles: this.state.bubbles.concat(bubble) });
    try {
      const result = await api.instanceSend(instanceId, prompt, attachments, mode);
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

  /** D-028 §5.3: interrupt the current turn; session and process stay alive. */
  async cancel(instanceId: Id) {
    await api.instanceCancel(instanceId);
    await this.refresh();
  }

  async sendKeys(instanceId: Id, key: PtyKey) {
    await api.instanceKeys(instanceId, key);
    await this.refreshScreen(instanceId);
  }

  async createWorktree(spec: WorktreeCreateSpec) {
    const record = await api.worktreeCreate(spec);
    await this.refreshHosts();
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

  /**
   * Continue an exited session on a new instance and return where to navigate.
   *
   * The exited instance keeps its history, so the caller must move the view to
   * the returned id or the user stays on a transcript that cannot accept input
   * (D-026).
   */
  async resume(instanceId: Id, mode: ResumeMode = "structured"): Promise<Id | null> {
    try {
      const result = await api.instanceResume(instanceId, mode);
      await this.refresh();
      return result.instanceId;
    } catch (error) {
      // The Hub's 409 explains *why* (no transcript / too old); showing it is
      // the difference between a dead button and an answer.
      this.toast(error instanceof Error ? error.message : "恢复会话失败");
      return null;
    }
  }

  /**
   * Real deletion. `force` is what stops a live Instance — the Hub does the
   * stop and the delete together, so the caller must not close it first.
   */
  async deleteInstance(instanceId: Id, force = false) {
    const result = await api.instanceDelete(instanceId, force);
    this.emit({ instances: this.state.instances.filter((row) => row.id !== instanceId) });
    await this.refresh();
    return result;
  }

  async configure(
    instanceId: Id,
    permissionMode: string,
    extras?: { model?: string; effort?: EffortSelection },
  ) {
    // The wire stores an opaque {name,index}; ultracode rides the legacy
    // "ultracode" name until x-p1-proto lands the {name,ultracode} shape.
    const wireExtras = extras?.effort
      ? { ...extras, effort: { name: effortWireName(extras.effort), index: extras.effort.index } }
      : extras;
    await api.instanceConfigure(instanceId, permissionMode, wireExtras);
    this.emit({
      permissionMode: { ...this.state.permissionMode, [instanceId]: permissionMode },
      ...(extras?.effort ? { effort: { ...this.state.effort, [instanceId]: extras.effort } } : {}),
      ...(extras?.model ? { models: { ...this.state.models, [instanceId]: extras.model } } : {}),
    });
  }

  async setEffort(instanceId: Id, effort: EffortSelection) {
    await this.configure(instanceId, this.permissionModeOf(instanceId), { effort });
  }

  async setModel(instanceId: Id, model: string) {
    await this.configure(instanceId, this.permissionModeOf(instanceId), { model });
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

  effortOf(instanceId: Id, kind?: EffortKind | string): EffortSelection {
    const stored = this.state.effort[instanceId];
    const instance = this.state.instances.find((row) => row.id === instanceId);
    const fallbackKind = (kind ?? stored?.kind ?? instance?.kind ?? "claude") as EffortKind;
    if (stored) {
      if (kind && stored.kind !== kind) return mapEffort(stored, kind);
      return stored;
    }
    const recorded = effortFromRecord(fallbackKind, instance?.effortName, instance?.effortIndex);
    if (recorded) return recorded;
    return effortAt(fallbackKind, readDeviceSettings().defaultEffortIndex ?? DEFAULT_EFFORT_INDEX);
  }

  /** §9.1: transcript-read-back effective effort, or `null` when unobserved. */
  effortEffectiveOf(instanceId: Id): EffortEffectiveView | null {
    return this.state.effortEffective[instanceId] ?? null;
  }

  /** The word the slider last requested for this instance (wire spelling). */
  effortRequestedWordOf(instanceId: Id): { word: string; ultracode: boolean } {
    const selected = this.state.effort[instanceId];
    const instance = this.state.instances.find((row) => row.id === instanceId);
    const fallback =
      effortFromRecord(
        (instance?.kind ?? "claude") as EffortKind,
        instance?.effortName,
        instance?.effortIndex,
        instance?.effortUltracode,
      ) ?? undefined;
    const current = selected ?? fallback;
    if (!current) return { word: "", ultracode: false };
    return { word: effortWireName(current), ultracode: current.ultracode === true };
  }

  modelOf(instanceId: Id, kind?: string): string {
    const instance = this.state.instances.find((row) => row.id === instanceId);
    return (
      this.state.models[instanceId] ??
      instance?.model ??
      (kind === "codex" ? "gpt-5" : kind === "grok" ? "grok-4" : "opus")
    );
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
