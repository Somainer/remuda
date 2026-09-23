import { useSyncExternalStore } from "react";

/** Bounded journal tail the list reads per live instance to project its phrase. */
const SUMMARY_TAIL = 64;
import type { Command } from "../types/command";
import type { Host, Instance } from "../types/instance";
import type { Interaction, InteractionAnswer } from "../types/interaction";
import type { Observation } from "../types/observation";
import type { Id, U64 } from "../types/wire";
import type { PromptMode } from "../types/generated";
import type { Workspace, WorkspaceSnapshot } from "../types/workspace";
import type { AttachmentRef } from "./attachments";
import { mapWorkspace, mergeHostWorkspaces } from "../features/workspaces/registry";
import {
  api,
  isScreenNodeBusy,
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
import type { UsageRollup } from "../features/session/contextUsage";
import {
  catalogFromRecord,
  modelFromObservation,
  modelFromRecord,
  type ModelCatalogView,
  type ModelEffectiveView,
} from "../features/session/modelEffective";
import {
  effectivePermissionFromObservation,
  effectivePermissionFromRecord,
  type PermissionEffectiveView,
} from "../features/session/permissionEffective";
import {
  normalizePermissionMode as normalizeKindPermissionMode,
} from "../features/session/permissions";
import { doneFromLines, lastLines, latestScreenFromObservations } from "./screen";
import { liveSummary } from "../features/session/liveSummary";
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

/** A push-down in flight (chip shows 切换中 / 排队中 until it settles). */
export type EffortPending = {
  /** Wire word the user chose (`low…max | ultracode`). */
  word: string;
  /** True while the agent is working — it applies at the next idle. */
  queued: boolean;
  /** Wall-clock ms the pending state was entered (stale-state safety net). */
  at: number;
  /**
   * §9.1 effective-level `observedAt` the push-down started from (null when
   * nothing had been read back). A read-back newer than this settles the
   * push-down; an equal/older projection leaves a queued push pending.
   */
  baselineObservedAt: string | null;
};

/** Pending entries older than this without a verdict are dropped. */
const EFFORT_PENDING_MAX_AGE_MS = 30 * 60_000;

/**
 * Bulk screen reads fan out one per listed row; cap the fan-out so even a long
 * list (the 2026-09-19 phone repro) stays far under the Hub's per-Node
 * bulk-read sub-budget and can never press the control reservation.
 */
const SCREEN_READ_CONCURRENCY = 4;

/** A process that is gone has no screen to read; never ask the Node for one. */
const SCREEN_SKIP_LIFECYCLES: ReadonlySet<Instance["lifecycle"]> = new Set(["exited", "failed"]);

/** A model switch in flight (mirrors EffortPending). */
export type ModelPending = {
  id: string;
  queued: boolean;
  at: number;
};

/** A permission-mode wheel walk in flight (chip shows 切换中 / 排队中). */
export type PermissionPending = {
  /** Wire mode the user chose. */
  mode: string;
  /** True while the agent is working — applies at the next idle. */
  queued: boolean;
  at: number;
};

/** Parse an `instance.configure` model lifecycle status the driver journals. */
function modelLifecycleStatus(status: unknown):
  | { kind: "queued" | "applied" | "degraded"; id: string; reason: string }
  | null {
  if (typeof status !== "string") return null;
  if (status.startsWith("model-queued:")) {
    return { kind: "queued", id: status.slice("model-queued:".length), reason: "" };
  }
  if (status.startsWith("model-applied:")) {
    return { kind: "applied", id: status.slice("model-applied:".length), reason: "" };
  }
  const prefix = "model-degraded:";
  if (status.startsWith(prefix)) {
    const rest = status.slice(prefix.length);
    const colon = rest.indexOf(":");
    if (colon < 0) return { kind: "degraded", id: rest, reason: "" };
    return { kind: "degraded", id: rest.slice(0, colon), reason: rest.slice(colon + 1) };
  }
  return null;
}

/** Parse an `instance.configure` permission lifecycle the driver journals. */
function permissionLifecycleStatus(status: unknown):
  | { kind: "queued" | "applied" | "degraded" | "unsupported"; mode: string; reason: string }
  | null {
  if (typeof status !== "string") return null;
  if (status.startsWith("permission-queued:")) {
    return { kind: "queued", mode: status.slice("permission-queued:".length), reason: "" };
  }
  if (status.startsWith("permission-applied:")) {
    return { kind: "applied", mode: status.slice("permission-applied:".length), reason: "" };
  }
  if (status.startsWith("permission-unsupported-in-session:")) {
    return {
      kind: "unsupported",
      mode: status.slice("permission-unsupported-in-session:".length),
      reason: "launch-only",
    };
  }
  const prefix = "permission-degraded:";
  if (status.startsWith(prefix)) {
    const rest = status.slice(prefix.length);
    const colon = rest.indexOf(":");
    if (colon < 0) return { kind: "degraded", mode: rest, reason: "" };
    return {
      kind: "degraded",
      mode: rest.slice(0, colon),
      reason: rest.slice(colon + 1),
    };
  }
  return null;
}

const PERMISSION_PENDING_MAX_AGE_MS = 30 * 60_000;

/** Parse an `instance.configure` effort lifecycle status the driver journals. */
function effortLifecycleStatus(status: unknown):
  | { kind: "queued" | "applied" | "degraded"; word: string; reason: string }
  | null {
  if (typeof status !== "string") return null;
  if (status.startsWith("effort-queued:")) {
    return { kind: "queued", word: status.slice("effort-queued:".length), reason: "" };
  }
  if (status.startsWith("effort-applied:")) {
    return { kind: "applied", word: status.slice("effort-applied:".length), reason: "" };
  }
  const prefix = "effort-degraded:";
  if (status.startsWith(prefix)) {
    const rest = status.slice(prefix.length);
    const colon = rest.indexOf(":");
    if (colon < 0) return { kind: "degraded", word: rest, reason: "" };
    return {
      kind: "degraded",
      word: rest.slice(0, colon),
      reason: rest.slice(colon + 1),
    };
  }
  return null;
}

export type ConnectionUi = "live" | "reconnecting" | "offline";
export type Toast = { id: string; text: string } | null;
export type LocalBubble = {
  /**
   * Local request identity, generated before the POST. It is **never** a
   * server `commandId` (exploration §5 P0-3): while it is the only id the
   * bubble has, the send is unconfirmed — it must not query `/v1/commands`
   * and must not be retried automatically.
   */
  clientRequestId: Id;
  instanceId: Id;
  text: string;
  /**
   * Server-assigned command identity. `null` until the POST response lands,
   * and stays `null` on the failure path — a 5xx or an offline Node leaves
   * the bubble in 「状态待确认」 rather than disguising the local id as a
   * server one.
   */
  commandId: Id | null;
  state: Command["state"] | "unknown";
  createdAt: string;
  /**
   * Thumbnails for images sent with this message (D-027). Held locally
   * because the Hub does not echo attachments back onto the journal yet.
   */
  attachments?: BubbleAttachment[];
  /** D-028 §6 PromptMode used for this send; absent is a normal new turn. */
  promptMode?: PromptMode;
  /**
   * c-steer: a Remuda-held queue row. Held messages have NOT been POSTed —
   * they wait for the running turn to end (or for a pending question to be
   * answered), render in the transcript as pending user rows with a reason
   * tag, can be cancelled locally, and are posted in order by
   * {@link HubStore.flushHeld}. The wire never sees them until they flush.
   */
  held?: boolean;
  /** Why a held row waits; drives its transcript tag. */
  holdReason?: "turn" | "answer";
  /** Manifest refs posted with the prompt when the hold flushes. */
  heldRefs?: AttachmentRef[];
};

/**
 * One file/image shown under a sent bubble. `index` is its 1-based
 * `[Image #n]`/`[File #n]` anchor (from the send manifest), so an inline
 * token can be paired with the chip.
 */
export type BubbleAttachment = {
  objectId: string;
  name: string;
  /** Blob URL for an optimistic image; files link straight to the Hub. */
  previewUrl: string;
  kind: "image" | "file";
  mediaType: string;
  size: number;
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
  /** context-usage-1 Hub-computed token/context rollup per instance.
   *  Hydrated separately from `instances` for the same reason effort is:
   *  mergeInstanceSnapshots keeps the local instance while its
   *  follow-bumped durableSeq is ahead, which would hide the polled row's
   *  fresh TPM windows. */
  usageRollup: Record<string, UsageRollup>;
  /** Effective permission mode per instance, read back from the TUI/transcript. */
  permissionEffective: Record<string, PermissionEffectiveView>;
  /** A permission wheel walk in flight (queued while the agent works). */
  permissionPending: Record<string, PermissionPending>;
  /** §9.1 a push-down in flight: `queued` while the agent works (applies at
   *  the next idle), `queued:false` on the idle fast path. Cleared when the
   *  effective read-back lands or the switch is rejected. */
  effortPending: Record<string, EffortPending>;
  models: Record<string, string>;
  /** §9.1 transcript-read-back effective model per instance. */
  modelEffective: Record<string, ModelEffectiveView>;
  /** §9.1 discovered switchable model list per instance. */
  modelCatalogs: Record<string, ModelCatalogView>;
  /** §9.1 a model switch in flight (切换中/排队中 until the verdict lands). */
  modelPending: Record<string, ModelPending>;
  compact: boolean;
  answering: Record<string, true>;
  screens: Record<string, { lines: string[]; done: boolean }>;
  /** List-row live phrases projected from each instance's journal tail. */
  summaries: Record<string, string>;
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
  usageRollup: {},
  effortPending: {},
  permissionEffective: {},
  permissionPending: {},
  models: {},
  modelEffective: {},
  modelCatalogs: {},
  modelPending: {},
  compact: typeof localStorage === "undefined" ? true : localStorage.getItem(COMPACT_KEY) !== "0",
  answering: {},
  screens: {},
  summaries: {},
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
  /**
   * §9.1: instanceId → effective `observedAt` of a push-down the durable Hub
   * projection settled before its live follow frame arrived. The matching live
   * observation must STILL be treated as our own push-down (leave the slider
   * where the user put it so a clamp mismatch stays visible), not as a
   * terminal-side switch that would fold the slider to the clamped level.
   */
  private settledEffortPushdown = new Map<Id, string>();
  private bootGen = 0;
  private pollTimer: ReturnType<typeof setInterval> | null = null;
  /** Screen-read scheduler: queue, single-flight set, in-flight counter. */
  private screenQueue: Id[] = [];
  private screenPending = new Set<Id>();
  private screenInFlight = 0;
  /** Per-row NODE_BUSY back-off: don't re-enqueue before this timestamp. */
  private screenBackoffUntil = new Map<Id, number>();
  private screenBackoffTimers = new Map<Id, ReturnType<typeof setTimeout>>();
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

  /**
   * Fold Hub-record `effortEffective` into the live map, newest wins.
   *
   * Also settles a push-down whose verdict has projected onto the durable
   * record. The live follow socket normally clears pending via
   * {@link noteEffortObservation}, but a frame that is lost to gap-backfill
   * (the client goes `readonly-stale` under load) or a late page attach only
   * reaches us through this poll/refresh path — without settling here the chip
   * would stay on 切换中 forever even though the Hub record already carries
   * the new level. Only a projection strictly newer than the level the
   * push-down started from settles, so a queued (working) switch is not
   * cleared by a poll returning the unchanged baseline.
   */
  private hydrateEffortEffective(instances: Instance[]) {
    const next = { ...this.state.effortEffective };
    const pendingNext = { ...this.state.effortPending };
    let effectiveUpdated = false;
    let pendingSettled = false;
    for (const instance of instances) {
      const view = effectiveFromRecord(instance.effortEffective);
      if (!view) continue;
      const current = next[instance.id];
      if (!current || view.observedAt >= current.observedAt) {
        next[instance.id] = view;
        effectiveUpdated = true;
        const pending = this.state.effortPending[instance.id];
        if (
          pending
          && (!pending.baselineObservedAt || view.observedAt > pending.baselineObservedAt)
        ) {
          delete pendingNext[instance.id];
          pendingSettled = true;
          // Remember the read-back that settled it so the same edge arriving
          // later on the live socket is not mistaken for a terminal switch.
          this.settledEffortPushdown.set(instance.id, view.observedAt);
        }
      }
    }
    if (effectiveUpdated || pendingSettled) {
      this.emit({
        ...(effectiveUpdated ? { effortEffective: next } : {}),
        ...(pendingSettled ? { effortPending: pendingNext } : {}),
      });
    }
  }

  /** context-usage-1: fold Hub-computed usage rollups from polled instances
   *  into the live map. The polled projection is always fresher than the
   *  retained one (its TPM windows are recomputed server-side at read
   *  time), so a rollup with at least as many turns wins. */
  private hydrateUsageRollups(instances: Instance[]) {
    let updated = false;
    const next = { ...this.state.usageRollup };
    for (const instance of instances) {
      const rollup = instance.usageRollup;
      if (!rollup) continue;
      const current = next[instance.id];
      if (!current || rollup.turns >= current.turns) {
        next[instance.id] = rollup;
        updated = true;
      }
    }
    if (updated) this.emit({ usageRollup: next });
  }

  /** §9.1: fold Hub-record modelEffective + modelCatalog into the live maps. */
  private hydrateModels(instances: Instance[]) {
    const effectiveNext = { ...this.state.modelEffective };
    const catalogNext = { ...this.state.modelCatalogs };
    let effUpdated = false;
    let catUpdated = false;
    for (const instance of instances) {
      const view = modelFromRecord(instance.modelEffective);
      if (view) {
        const current = effectiveNext[instance.id];
        if (!current || view.observedAt >= current.observedAt) {
          effectiveNext[instance.id] = view;
          effUpdated = true;
        }
      }
      const catalog = catalogFromRecord(instance.modelCatalog);
      if (catalog) {
        catalogNext[instance.id] = catalog;
        catUpdated = true;
      }
    }
    if (effUpdated || catUpdated) {
      this.emit({
        ...(effUpdated ? { modelEffective: effectiveNext } : {}),
        ...(catUpdated ? { modelCatalogs: catalogNext } : {}),
      });
    }
  }

  /** Apply one transcript-read-back model observation: settles a pending
   *  push-down, records the discovered catalog, and — for a terminal-side
   *  switch — moves the picker selection locally without a configure. */
  /** Apply one transcript-read-back model observation. `live` events settle a
   *  pending push-down and fold terminal-side switches; history replay only
   *  hydrates effective/catalog state (it must never consume a pending set
   *  after the replay window started, nor move the optimistic selection). */
  private noteModelObservation(instanceId: Id, observation: Observation, live: boolean): boolean {
    const parsed = modelFromObservation(observation);
    if (!parsed) return false;
    const current = this.state.modelEffective[instanceId];
    if (current && parsed.effective.observedAt < current.observedAt) return false;
    const patch: Partial<HubState> = {
      modelEffective: {
        ...this.state.modelEffective,
        [instanceId]: parsed.effective,
      },
    };
    if (parsed.catalog) {
      patch.modelCatalogs = {
        ...this.state.modelCatalogs,
        [instanceId]: parsed.catalog,
      };
    }
    // A catalog-only refresh edge (the scoped gateway cache landed after
    // promotion; no `requested`, so it is not a switch verdict) hydrates the
    // list but must never settle an in-flight pending or move the optimistic
    // selection.
    const catalogOnly = Boolean(parsed.catalog) && !parsed.hasRequested;
    if (live && this.state.modelPending[instanceId] && !catalogOnly) {
      // Our own push-down settled: keep the optimistic selection; the mismatch
      // line renders if the resolved id differs.
      patch.modelPending = { ...this.state.modelPending };
      delete patch.modelPending[instanceId];
    } else if (live && !catalogOnly) {
      // Live, terminal-side switch: fold the observed id into the local
      // selection so a hand-typed `/model` moves the picker, never calling
      // configure back.
      patch.models = { ...this.state.models, [instanceId]: parsed.effective.id };
    } else if (!live && this.state.models[instanceId] == null) {
      // History replay on a fresh mount: seed the selection from the observed
      // id so the picker reflects the resolved model after reload.
      patch.models = { ...this.state.models, [instanceId]: parsed.effective.id };
    }
    this.emit(patch);
    return true;
  }

  /** Fold one `instance.configure` model lifecycle into the pending map. Only
   *  live lifecycle events settle pending/queued; replayed history hydrates
   *  nothing here (the observation reducer already carried the edge). */
  private noteModelLifecycle(instanceId: Id, observation: Observation, live = true) {
    const payload = observation.payload as
      | { type?: string; nativeName?: string; status?: unknown }
      | undefined;
    if (payload?.type !== "native" || payload.nativeName !== "instance.configure") return;
    const value =
      typeof payload.status === "string"
        ? payload.status
        : payload.status && typeof payload.status === "object" && "value" in payload.status
          ? (payload.status as { value: unknown }).value
          : undefined;
    const parsed = modelLifecycleStatus(value);
    if (!parsed || !live) return;
    const pending = { ...this.state.modelPending };
    if (parsed.kind === "queued") {
      pending[instanceId] = { id: parsed.id, queued: true, at: Date.now() };
      this.emit({ modelPending: pending });
      return;
    }
    if (!pending[instanceId] && parsed.kind === "applied") return;
    delete pending[instanceId];
    if (parsed.kind === "degraded") {
      // Refused: revert the picker to the last observed id (or drop the
      // optimistic request so the instance default returns).
      const effective = this.state.modelEffective[instanceId];
      const models = { ...this.state.models };
      if (effective) models[instanceId] = effective.id;
      else delete models[instanceId];
      const reason =
        { "not-found": "模型不存在", "dialog-kept": "已取消切换", "no-readback-within-window": "未收到回读" }[
          parsed.reason
        ] ?? parsed.reason;
      this.toast(`模型切换被拒绝：${reason}`);
      this.emit({ modelPending: pending, models });
    } else {
      this.emit({ modelPending: pending });
    }
  }

  /** Fold Hub-record `permissionEffective` into the live map. */
  private hydratePermissionEffective(instances: Instance[]) {
    let updated = false;
    const next = { ...this.state.permissionEffective };
    for (const instance of instances) {
      const view = effectivePermissionFromRecord(instance.permissionEffective);
      if (!view) continue;
      const current = next[instance.id];
      if (!current || view.observedAt >= current.observedAt) {
        next[instance.id] = view;
        updated = true;
      }
    }
    if (updated) this.emit({ permissionEffective: next });
  }

  /** Apply one transcript-read-back effort observation to the live map.
   *  Settles any pending push-down and, when the change came from the
   *  terminal side, moves the slider to the observed stop (single source of
   *  truth; never calls configure, so no push-down ping-pong). */
  private noteEffortObservation(instanceId: Id, observation: Observation): boolean {
    const parsed = effectiveFromObservation(observation);
    if (!parsed) return false;
    const current = this.state.effortEffective[instanceId];
    if (current && parsed.effective.observedAt < current.observedAt) return false;
    const patch: Partial<HubState> = {
      effortEffective: {
        ...this.state.effortEffective,
        [instanceId]: parsed.effective,
      },
    };
    const pending = this.state.effortPending[instanceId];
    const hydratedAt = this.settledEffortPushdown.get(instanceId);
    // "Ours" = a live push-down still pending, OR one the durable projection
    // already settled whose live frame is only now arriving (same read-back,
    // observedAt no newer than the one the poll folded).
    const ours = Boolean(pending) || (hydratedAt != null && parsed.effective.observedAt <= hydratedAt);
    if (ours) {
      // Our own push-down settled: leave the slider where the user put it (the
      // mismatch line renders if the native side clamped it).
      if (pending) {
        patch.effortPending = { ...this.state.effortPending };
        delete patch.effortPending[instanceId];
      }
      // Consume the marker once the matching (or an even newer) live edge for
      // our push-down arrives; a genuinely newer terminal switch (observedAt
      // past the marker) is handled in the else branch instead.
      if (hydratedAt != null && parsed.effective.observedAt >= hydratedAt) {
        this.settledEffortPushdown.delete(instanceId);
      }
    } else {
      // Terminal-side switch (or a level newer than any push-down we settled):
      // the observed level is the truth — move the slider to it. This is local
      // state only, so it cannot re-trigger a configure.
      this.settledEffortPushdown.delete(instanceId);
      const instance = this.state.instances.find((row) => row.id === instanceId);
      const kind = (instance?.kind ?? "claude") as EffortKind;
      const selection = effortFromRecord(
        kind,
        parsed.effective.name,
        null,
        parsed.effective.ultracode === true,
      );
      if (selection) {
        const stored = this.state.effort[instanceId];
        if (
          !stored
          || stored.name !== selection.name
          || (stored.ultracode === true) !== (selection.ultracode === true)
        ) {
          patch.effort = { ...this.state.effort, [instanceId]: selection };
        }
      }
    }
    this.emit(patch);
    return true;
  }

  /** Fold one `instance.configure` effort lifecycle into the pending map. */
  private noteEffortLifecycle(instanceId: Id, observation: Observation) {
    const payload = observation.payload as
      | { type?: string; nativeName?: string; status?: unknown }
      | undefined;
    if (payload?.type !== "native" || payload.nativeName !== "instance.configure") return;
    // The journal persists status as a bare string; newer runs may wrap it.
    const value =
      typeof payload.status === "string"
        ? payload.status
        : payload.status && typeof payload.status === "object" && "value" in payload.status
          ? (payload.status as { value: unknown }).value
          : undefined;
    const parsed = effortLifecycleStatus(value);
    if (!parsed) return;
    const pending = { ...this.state.effortPending };
    if (parsed.kind === "queued") {
      // Preserve the baseline the push-down started from so the queued entry
      // is settled only by a strictly newer read-back, not a poll returning
      // the unchanged level.
      pending[instanceId] = {
        word: parsed.word,
        queued: true,
        at: pending[instanceId]?.at ?? Date.now(),
        baselineObservedAt: pending[instanceId]?.baselineObservedAt ?? null,
      };
      this.emit({ effortPending: pending });
      return;
    }
    if (!pending[instanceId] && parsed.kind === "applied") return;
    delete pending[instanceId];
    // The push-down ended via a lifecycle, not a projected read-back: no settled
    // edge owes the later live frame the "our push-down" treatment.
    this.settledEffortPushdown.delete(instanceId);
    if (parsed.kind === "degraded") {
      // The native side refused: revert the slider to the last observed level
      // (or drop the optimistic request so the record default returns).
      const effective = this.state.effortEffective[instanceId];
      const effort = { ...this.state.effort };
      if (effective) {
        const instance = this.state.instances.find((row) => row.id === instanceId);
        const kind = (instance?.kind ?? "claude") as EffortKind;
        const selection = effortFromRecord(
          kind,
          effective.name,
          null,
          effective.ultracode === true,
        );
        if (selection) effort[instanceId] = selection;
      } else {
        delete effort[instanceId];
      }
      const reason =
        { "dialog-kept": "已取消切换", "invalid-argument": "档位无效", "no-readback-within-window": "未收到回读" }[
          parsed.reason
        ] ?? parsed.reason;
      this.toast(`effort 切换被拒绝：${reason}`);
      this.emit({ effortPending: pending, effort });
    } else {
      this.emit({ effortPending: pending });
    }
  }

  toast(text: string) {
    this.emit({ toast: { id: String(Date.now()), text } });
  }

  /** Fold one effective permission-mode observation. A pending Remuda walk
   *  settles; a terminal-side change moves the chip locally without ever
   *  calling configure back (no ping-pong). */
  private notePermissionObservation(instanceId: Id, observation: Observation): boolean {
    const parsed = effectivePermissionFromObservation(observation);
    if (!parsed) return false;
    const current = this.state.permissionEffective[instanceId];
    if (current && parsed.effective.observedAt < current.observedAt) return false;
    const patch: Partial<HubState> = {
      permissionEffective: {
        ...this.state.permissionEffective,
        [instanceId]: parsed.effective,
      },
    };
    const pending = this.state.permissionPending[instanceId];
    if (pending) {
      patch.permissionPending = { ...this.state.permissionPending };
      delete patch.permissionPending[instanceId];
    } else {
      // Terminal-side change (shift+tab / /plan with no pending walk): move
      // the requested mode to the observed word locally.
      const instance = this.state.instances.find((row) => row.id === instanceId);
      const kind = instance?.kind ?? "claude";
      const mode = normalizeKindPermissionMode(kind, parsed.effective.mode);
      if (this.state.permissionMode[instanceId] !== mode) {
        patch.permissionMode = {
          ...this.state.permissionMode,
          [instanceId]: mode,
        };
      }
    }
    this.emit(patch);
    return true;
  }

  /** Fold one `instance.configure` permission lifecycle. */
  private notePermissionLifecycle(instanceId: Id, observation: Observation) {
    const payload = observation.payload as
      | { type?: string; nativeName?: string; status?: unknown }
      | undefined;
    if (payload?.type !== "native" || payload.nativeName !== "instance.configure") return;
    const value =
      typeof payload.status === "string"
        ? payload.status
        : payload.status && typeof payload.status === "object" && "value" in payload.status
          ? (payload.status as { value: unknown }).value
          : undefined;
    const parsed = permissionLifecycleStatus(value);
    if (!parsed) return;
    const pending = { ...this.state.permissionPending };
    if (parsed.kind === "queued") {
      pending[instanceId] = { mode: parsed.mode, queued: true, at: Date.now() };
      this.emit({ permissionPending: pending });
      return;
    }
    if (!pending[instanceId] && parsed.kind === "applied") return;
    delete pending[instanceId];
    if (parsed.kind === "degraded" || parsed.kind === "unsupported") {
      // Revert the optimistic chip to the last observed mode.
      const effective = this.state.permissionEffective[instanceId];
      const permissionMode = { ...this.state.permissionMode };
      if (effective) {
        const instance = this.state.instances.find((row) => row.id === instanceId);
        permissionMode[instanceId] = normalizeKindPermissionMode(
          instance?.kind ?? "claude",
          effective.mode,
        );
      } else {
        delete permissionMode[instanceId];
      }
      const reason =
        ({
          "dialog-kept": "已取消切换",
          "bypass-dialog-kept": "绕过确认已取消",
          "launch-only": "仅启动时可选",
          "no-status-line": "未读到状态行",
          "landed-other": "状态行未确认目标模式",
          "no-readback-within-window": "未收到回读",
          "screen-unreadable": "屏幕不可读",
        } as Record<string, string>)[parsed.reason] ?? parsed.reason;
      this.toast(`权限模式切换被拒绝：${reason}`);
      this.emit({ permissionPending: pending, permissionMode });
    } else {
      this.emit({ permissionPending: pending });
    }
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
      this.hydrateUsageRollups(instances.items);
      this.hydrateModels(instances.items);
      this.hydratePermissionEffective(instances.items);
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
    this.hydrateUsageRollups(instances.items);
    this.hydrateModels(instances.items);
    this.hydratePermissionEffective(instances.items);
    // NOTE: list-row live phrases are NOT derived here. refresh() fans into
    // every authenticated path (close/cancel/create/resume re-enter it) and
    // must not add journal polling; the mounted SessionList hydrates phrases
    // for the rows it renders via hydrateRowSummaries().
  }

  /** durableSeq already projected for a row phrase; unchanged seq = skip. */
  private summarySeqs = new Map<Id, string>();
  /** Ids this store currently projects phrases for (reconciled each tick). */
  private summaryManaged = new Set<Id>();
  /** Coalesces overlapping ticks so journal reads never overlap. */
  private summariesInFlight: Promise<void> | null = null;

  /**
   * Project one live phrase per rendered list row from a bounded journal tail.
   * Driven by the mounted SessionList poll (not refresh): the call is scoped
   * to rendered row ids, skips instances whose durableSeq is unchanged, never
   * overlaps itself, and clears phrases that no longer apply (finished run,
   * row no longer rendered), so a row always falls back to its constant text
   * rather than keeping a stale invented phrase.
   */
  hydrateRowSummaries(rowIds: Id[]): Promise<void> {
    if (this.summariesInFlight) return this.summariesInFlight;
    const job = this.projectRowSummaries(rowIds).finally(() => {
      if (this.summariesInFlight === job) this.summariesInFlight = null;
    });
    this.summariesInFlight = job;
    return job;
  }

  private async projectRowSummaries(rowIds: Id[]): Promise<void> {
    const rows = rowIds.flatMap((id) => this.state.instances.find((row) => row.id === id) ?? []);
    const current = new Map(rows.map((row) => [row.id, row]));
    const results = await Promise.all(
      rows.map(async (instance) => {
        const seq = String(instance.durableSeq ?? "0");
        if (this.summarySeqs.get(instance.id) === seq) {
          return { instance, phrase: this.state.summaries[instance.id], unchanged: true } as const;
        }
        try {
          // One bounded read: the journal endpoint serves the newest window
          // (≤2000 rows ascending). Open at durable-N so the tail stays small.
          const from = Math.max(0, Number(seq) - SUMMARY_TAIL);
          const page = await api.eventsRead({
            journalId: instance.journalId,
            afterSeq: String(from) as U64,
            limit: SUMMARY_TAIL,
          });
          this.summarySeqs.set(instance.id, seq);
          return { instance, phrase: liveSummary(page.events) ?? "", unchanged: false } as const;
        } catch {
          return null;
        }
      }),
    );

    const next = { ...this.state.summaries };
    let changed = false;
    // Add/update the projected phrases for this render and drop keys whose
    // row is no longer managed (space switch / unmount) so they cannot stick.
    for (const id of this.summaryManaged) {
      if (!current.has(id) && id in next) {
        delete next[id];
        this.summarySeqs.delete(id);
        changed = true;
      }
    }
    this.summaryManaged = new Set(current.keys());
    for (const result of results) {
      if (!result || result.unchanged) continue;
      const { instance, phrase } = result;
      if (phrase) {
        if (next[instance.id] !== phrase) {
          next[instance.id] = phrase;
          changed = true;
        }
      } else if (instance.id in next) {
        delete next[instance.id];
        changed = true;
      }
    }
    if (changed) this.emit({ summaries: next });
  }

  async follow(instanceId: Id) {
    const instance = this.state.instances.find((i) => i.id === instanceId) ?? (await api.instanceGet(instanceId));
    if (!this.state.instances.some((i) => i.id === instanceId)) {
      this.emit({ instances: [instance, ...this.state.instances] });
    }
    // Fold projected effective state/catalog the single record carries.
    this.hydrateEffortEffective([instance]);
    this.hydrateModels([instance]);
    if (this.journals.has(instance.journalId)) return;
    this.emit({ journalStatus: { ...this.state.journalStatus, [instanceId]: "live" } });
    // Seed from ONE bounded tail window (newest rows win). The old ascending
    // 512-loop sliced the front of the tail, silently dropping everything
    // below the middle of the window on a long journal. Older rows load on
    // demand via JournalClient.loadEarlier.
    const seed = await api.eventsRead({ journalId: instance.journalId, limit: 2000 });
    // Another mount may finish loading this journal while this read is pending.
    if (this.journals.has(instance.journalId)) return;
    const history = seed.events;
    const historyPhrase = liveSummary(history);
    this.emit({
      instances: applyInstanceActivity(this.state.instances, history),
      events: { ...this.state.events, [instanceId]: history },
      summaries: historyPhrase
        ? { ...this.state.summaries, [instanceId]: historyPhrase }
        : this.state.summaries,
    });
    // §9.1: the Hub record usually already carries the latest effective level;
    // replay history effort edges too so a reconnect before refresh is honest.
    for (const event of history) {
      this.noteEffortObservation(instanceId, event);
      this.noteEffortLifecycle(instanceId, event);
      // History replay hydrates observed model state only; it must not settle a
      // push-down pending or fold the selection (those belong to live events).
      this.noteModelObservation(instanceId, event, false);
      this.noteModelLifecycle(instanceId, event, false);
      this.notePermissionObservation(instanceId, event);
      this.notePermissionLifecycle(instanceId, event);
    }
    const read: JournalRead = async (args) => {
      if (args.journalId === mockJournalIds.journalGap && args.afterSeq && Number(args.afterSeq) > 0) {
        await new Promise((resolve) => setTimeout(resolve, 500));
      }
      return api.eventsRead(args);
    };
    const last = history.at(-1)?.seq ?? ("0" as Observation["seq"]);
    const seedAsOf = (Number(seed.durableSeq) >= Number(last) ? seed.durableSeq : last) as Observation["seq"];
    const client = new JournalClient(instance.journalId, read, {
      onEvents: (events) => {
        const current = this.state.events[instanceId] ?? [];
        const seen = new Set(current.map((e) => e.eventId));
        const fresh = events.filter((e) => !seen.has(e.eventId));
        // §9.1: live effort edges update the effective level immediately —
        // the slider reflects the transcript, not the optimistic request.
        for (const event of fresh) {
          this.noteEffortObservation(instanceId, event);
          this.noteEffortLifecycle(instanceId, event);
          this.noteModelObservation(instanceId, event, true);
          this.noteModelLifecycle(instanceId, event, true);
          this.notePermissionObservation(instanceId, event);
          this.notePermissionLifecycle(instanceId, event);
        }
        const next = current.concat(fresh);
        const screen = latestScreenFromObservations(next);
        const phrase = liveSummary(next);
        // Clear a finished run's phrase so the row falls back to its
        // constant sentence instead of keeping a stale (invented) status.
        const summaries = { ...this.state.summaries };
        if (phrase) summaries[instanceId] = phrase;
        else delete summaries[instanceId];
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
          summaries,
        });
      },
      onPrepend: (older) => {
        const current = this.state.events[instanceId] ?? [];
        const seen = new Set(current.map((e) => e.eventId));
        const fresh = older.filter((e) => !seen.has(e.eventId));
        if (!fresh.length) return;
        // Load-earlier rows land above every loaded node. Merge by seq rather
        // than trusting arrival order: assemble/Transcript anchor on it.
        const merged = current.concat(fresh).sort((a, b) => Number(a.seq) - Number(b.seq));
        for (const event of fresh) {
          this.noteEffortObservation(instanceId, event);
          this.noteEffortLifecycle(instanceId, event);
          this.noteModelObservation(instanceId, event, false);
          this.noteModelLifecycle(instanceId, event, false);
          this.notePermissionObservation(instanceId, event);
          this.notePermissionLifecycle(instanceId, event);
        }
        this.emit({
          instances: applyInstanceActivity(this.state.instances, fresh),
          events: { ...this.state.events, [instanceId]: merged },
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
      asOfSeq: seedAsOf,
      instance: {} as Instance,
      runs: [],
      commands: [],
      pendingInteractions: [],
      nodes: [],
      // The seed is a bounded window: its floor is a window floor, and
      // complete=false says older rows remain behind load-earlier. It is NOT
      // fed as a retention floor anywhere (JournalClient uses it only as the
      // load-earlier anchor and live-batch stale check).
      history: { earliestRetainedSeq: seed.windowFromSeq ?? "1", complete: seed.reachedAfterSeq },
    });
    const sub = await api.eventsSubscribe(
      instance.journalId,
      last,
      (batch) => {
        const result = client.applyBatch(batch);
        if (result.acked) void api.eventsAck(batch.subscriptionId, instance.journalId, result.acked);
        if (result.gap) void client.fillGap(result.gap.from, result.gap.to);
      },
      (windowFloor) => void client.fillResyncGap(windowFloor),
    );
    this.subs.set(instance.journalId, sub.subscriptionId);
    if (Number(sub.snapshot.asOfSeq) >= Number(last)) {
      // An EMPTY follow snapshot only says "no events past the afterSeq
      // cursor" — it says nothing about retention below it. Preserve the REST
      // seed's window floor/completeness instead of letting an empty snapshot
      // reset the floor to 1 (which would hide load-earlier on a late attach).
      const seedHistory =
        sub.windowFromSeq === null
          ? { earliestRetainedSeq: seed.windowFromSeq ?? "1", complete: seed.reachedAfterSeq }
          : sub.snapshot.history;
      client.applySnapshot({ ...sub.snapshot, history: seedHistory });
    }
    const tail = mockGappedTail(instance.journalId);
    if (tail) {
      setTimeout(() => {
        const result = client.applyBatch({ ...tail, subscriptionId: sub.subscriptionId });
        if (result.gap) void client.fillGap(result.gap.from, result.gap.to);
      }, 0);
    }
    // Events landing between the REST seed and the socket open arrive on the
    // follow snapshot (filtered past `last`) and flow through applyBatch, so no
    // second tail read is needed.
  }

  /** Fetch one window of older history for the transcript's load-earlier row. */
  async loadEarlier(instanceId: Id) {
    const instance = this.state.instances.find((i) => i.id === instanceId);
    if (!instance) return null;
    const client = this.journals.get(instance.journalId);
    if (!client) return null;
    return client.loadEarlier();
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
  ): Promise<boolean> {
    // Anchor mapping (D-027 + image anchors): the manifest carries the
    // [Image #n] index in token order; pair it onto the local bubble's
    // previews so the token renders as an inline thumbnail chip.
    const indexOf = new Map(attachments.map((ref) => [ref.objectId, ref.index]));
    const numberedPreviews = previews.map((preview) => {
      const index = indexOf.get(preview.objectId);
      return index ? { ...preview, index } : preview;
    });
    const clientRequestId = id("local_");
    const bubble: LocalBubble = {
      clientRequestId,
      instanceId,
      text: prompt,
      ...(numberedPreviews.length ? { attachments: numberedPreviews } : {}),
      // No server identity yet; the projection below reads this null as
      // 「等待发送」 while queued and 「状态待确认」 afterwards, never as
      // delivery. (C2: the local id must never masquerade as a commandId.)
      commandId: null,
      state: "queued",
      ...(mode ? { promptMode: mode } : {}),
      createdAt: now(),
    };
    this.emit({ bubbles: this.state.bubbles.concat(bubble) });
    try {
      const result = await api.instanceSend(instanceId, prompt, attachments, mode);
      // The ONLY place commandId is assigned: the server response.
      this.emit({
        bubbles: this.state.bubbles.map((b) =>
          b.clientRequestId === clientRequestId
            ? { ...b, state: result.command.state, commandId: result.command.commandId }
            : b,
        ),
      });
      await this.catchup(instanceId);
      const events = this.state.events[instanceId] ?? [];
      this.emit({ bubbles: settleBubbles(this.state.bubbles, instanceId, events) });
      await this.refreshScreen(instanceId).catch(() => undefined);
      return true;
    } catch {
      // Keep `commandId: null`. The bubble is 「状态待确认」: no command id
      // to query with, and nothing here re-POSTs. Recovering the send is a
      // human decision taken from the transcript, not an automatic retry.
      this.emit({
        bubbles: this.state.bubbles.map((b) =>
          b.clientRequestId === clientRequestId ? { ...b, state: "unknown" } : b,
        ),
      });
      return false;
    }
  }

  retract(bubbleId: Id) {
    const bubble = this.state.bubbles.find((b) => b.clientRequestId === bubbleId);
    if (!bubble || bubble.state === "accepted" || bubble.state === "settled") return;
    this.emit({ bubbles: this.state.bubbles.filter((b) => b.clientRequestId !== bubbleId) });
  }

  /**
   * c-steer: hold a prompt client-side (Enter while working / while a question
   * is pending). Nothing is POSTed; the row shows why it waits and can be
   * cancelled. {@link flushHeld} posts the rows in order.
   */
  hold(
    instanceId: Id,
    prompt: string,
    reason: "turn" | "answer",
    refs: AttachmentRef[] = [],
    previews: BubbleAttachment[] = [],
  ): Id {
    const clientRequestId = id("local_");
    const bubble: LocalBubble = {
      clientRequestId,
      instanceId,
      text: prompt,
      commandId: null,
      state: "queued",
      promptMode: "queue",
      held: true,
      holdReason: reason,
      heldRefs: refs,
      ...(previews.length ? { attachments: previews } : {}),
      createdAt: now(),
    };
    this.emit({ bubbles: this.state.bubbles.concat(bubble) });
    return clientRequestId;
  }

  /** Held rows for one instance, in queue order (oldest first). */
  heldBubbles(instanceId: Id): LocalBubble[] {
    return this.state.bubbles.filter((b) => b.instanceId === instanceId && b.held && b.state === "queued");
  }

  /**
   * Post every held prompt as an ordinary new turn, in queue order — the
   * working→idle (or blocked→working/idle) transition handler. Each row loses
   * its held tag the moment its POST lands; a failed POST leaves that row as
   * 状态待确认, exactly like a direct send failure, never silently dropped.
   *
   * The snapshot is taken for ORDER only: a row is re-read when its turn comes,
   * because a 插队发送 (steerHeld) or a retract can land while an earlier row's
   * POST is still in flight — notably the blocked→working answered flush runs
   * while every remaining row shows an enabled 插队发送. Such a row already has
   * its own POST in flight, so posting it again here would double-send it; the
   * fresh `held && queued` check skips it instead.
   */
  async flushHeld(instanceId: Id) {
    const items = this.heldBubbles(instanceId);
    for (const item of items) {
      const current = this.state.bubbles.find((b) => b.clientRequestId === item.clientRequestId);
      if (!current || !current.held || current.state !== "queued") continue;
      // Drop the hold marker before the POST so a second transition cannot
      // double-send the same row.
      this.emit({
        bubbles: this.state.bubbles.map((b) =>
          b.clientRequestId === item.clientRequestId
            ? { ...b, held: false, promptMode: "new-turn" as const }
            : b,
        ),
      });
      try {
        const result = await api.instanceSend(instanceId, item.text, item.heldRefs ?? []);
        this.emit({
          bubbles: this.state.bubbles.map((b) =>
            b.clientRequestId === item.clientRequestId
              ? { ...b, state: result.command.state, commandId: result.command.commandId }
              : b,
          ),
        });
      } catch {
        this.emit({
          bubbles: this.state.bubbles.map((b) =>
            b.clientRequestId === item.clientRequestId ? { ...b, state: "unknown" } : b,
          ),
        });
      }
    }
    await this.catchup(instanceId);
  }

  /**
   * c-steer 插队发送: take ONE already-held row (see {@link hold}) and send it
   * NOW as a steer — the Node interrupts the running turn and jumps this
   * message ahead of the rest of the held queue. The held marker is dropped and
   * `promptMode` set to steer BEFORE the POST, reusing {@link flushHeld}'s
   * double-send guard so a transition firing mid-flight cannot re-send it. A
   * failed POST leaves the row 状态待确认 exactly like flushHeld's catch, never
   * silently dropped; a second call for the same id finds no held row and is a
   * no-op. The remaining held rows keep their order and their ordinals.
   *
   * Resolves `true` only once the steer POST has actually landed (so the UI can
   * show 已打断 as a receipt rather than an intent); `false` on a no-op or a
   * failed POST.
   */
  async steerHeld(instanceId: Id, bubbleId: Id): Promise<boolean> {
    const item = this.state.bubbles.find(
      (b) => b.clientRequestId === bubbleId && b.instanceId === instanceId && b.held && b.state === "queued",
    );
    if (!item) return false;
    this.emit({
      bubbles: this.state.bubbles.map((b) =>
        b.clientRequestId === bubbleId ? { ...b, held: false, promptMode: "steer" as const } : b,
      ),
    });
    try {
      const result = await api.instanceSend(instanceId, item.text, item.heldRefs ?? [], "steer");
      this.emit({
        bubbles: this.state.bubbles.map((b) =>
          b.clientRequestId === bubbleId
            ? { ...b, state: result.command.state, commandId: result.command.commandId }
            : b,
        ),
      });
      await this.catchup(instanceId);
      return true;
    } catch {
      this.emit({
        bubbles: this.state.bubbles.map((b) =>
          b.clientRequestId === bubbleId ? { ...b, state: "unknown" } : b,
        ),
      });
      return false;
    }
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
    await this.refreshScreen(instanceId).catch(() => undefined);
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
    let read;
    try {
      read = await api.screenRead(instanceId, 80);
    } catch (err) {
      // NODE_BUSY is a refusal, not a screen: let the scheduler back off.
      if (isScreenNodeBusy(err)) throw err;
      read = { lines: [] };
    }
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

  /**
   * Poll screens for a batch of rows under the bulk-read contract:
   *
   * - exited/failed instances are never asked (the process is gone);
   * - at most {@link SCREEN_READ_CONCURRENCY} reads are in flight and each
   *   row is single-flight, so the 2.5 s list poll cannot stack duplicates;
   * - ids keep the caller's order — the list passes visible rows first;
   * - a NODE_BUSY refusal parks the row for the Hub's retry hint instead of
   *   logging or surfacing an error.
   *
   * Fire-and-forget on purpose: the list polls on an interval and must never
   * await a fan-out.
   */
  refreshScreens(instanceIds: Id[]) {
    const now = Date.now();
    for (const id of instanceIds) {
      if (this.screenPending.has(id)) continue;
      if (now < (this.screenBackoffUntil.get(id) ?? 0)) continue;
      const instance = this.state.instances.find((row) => row.id === id);
      if (!instance || SCREEN_SKIP_LIFECYCLES.has(instance.lifecycle)) continue;
      this.screenPending.add(id);
      this.screenQueue.push(id);
    }
    this.pumpScreenReads();
  }

  /**
   * Synchronous admission loop: it only launches (never awaits) reads, so the
   * in-flight counter cannot race between two callers. Completion callbacks
   * re-pump from outside this frame.
   */
  private pumpScreenReads() {
    while (this.screenInFlight < SCREEN_READ_CONCURRENCY) {
      const id = this.screenQueue.shift();
      if (!id) break;
      // Re-check at dequeue: a row may have exited while queued behind others.
      const instance = this.state.instances.find((row) => row.id === id);
      if (!instance || SCREEN_SKIP_LIFECYCLES.has(instance.lifecycle)) {
        this.screenPending.delete(id);
        continue;
      }
      this.screenInFlight += 1;
      void this
        .runScreenRead(id)
        .catch(() => undefined)
        .finally(() => {
          this.screenInFlight -= 1;
          this.screenPending.delete(id);
          this.pumpScreenReads();
        });
    }
  }

  private async runScreenRead(instanceId: Id) {
    try {
      await this.refreshScreen(instanceId);
    } catch (err) {
      if (!isScreenNodeBusy(err)) return;
      // Back off exactly this row for one poll cycle; intervening interval
      // ticks skip it via screenBackoffUntil, and no error reaches the console.
      const retryAfterMs = err.retryAfterMs;
      this.screenBackoffUntil.set(instanceId, Date.now() + retryAfterMs);
      const oldTimer = this.screenBackoffTimers.get(instanceId);
      if (oldTimer) clearTimeout(oldTimer);
      const timer = setTimeout(() => {
        this.screenBackoffTimers.delete(instanceId);
        this.screenBackoffUntil.delete(instanceId);
        this.refreshScreens([instanceId]);
      }, retryAfterMs);
      this.screenBackoffTimers.set(instanceId, timer);
    }
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

  /** §9.1: pending effort push-down for an instance, if any. */
  effortPendingOf(instanceId: Id): EffortPending | null {
    const pending = this.state.effortPending[instanceId];
    if (pending && Date.now() - pending.at > EFFORT_PENDING_MAX_AGE_MS) {
      const next = { ...this.state.effortPending };
      delete next[instanceId];
      this.state = { ...this.state, effortPending: next };
      return null;
    }
    return pending ?? null;
  }

  async setEffort(instanceId: Id, effort: EffortSelection) {
    const word = effortWireName(effort);
    const instance = this.state.instances.find((row) => row.id === instanceId);
    const busy =
      instance?.activity?.state === "known" ? instance.activity.value === "working" : false;
    // Optimistic pending so the chip never shows the old level ambiguously:
    // 切换中 on the idle fast path, 排队中 while the agent works. The driver's
    // `effort-queued` lifecycle and the effective read-back settle it.
    this.settledEffortPushdown.delete(instanceId);
    this.emit({
      effortPending: {
        ...this.state.effortPending,
        [instanceId]: {
          word,
          queued: busy,
          at: Date.now(),
          baselineObservedAt: this.state.effortEffective[instanceId]?.observedAt ?? null,
        },
      },
    });
    try {
      await this.configure(instanceId, this.permissionModeOf(instanceId), { effort });
      // Re-request authoritative state once the command is accepted. The
      // driver projects the read-back before acking, so folding the durable
      // record settles the chip from the Hub even when this client's live
      // follow frame was gapped/coalesced; the 2s poll keeps settling it if
      // this read races. Bounded by one network round trip — no timer.
      await this.refresh().catch(() => undefined);
    } catch (error) {
      const pending = { ...this.state.effortPending };
      delete pending[instanceId];
      this.emit({ effortPending: pending });
      throw error;
    }
  }

  async setModel(instanceId: Id, model: string) {
    const instance = this.state.instances.find((row) => row.id === instanceId);
    const busy =
      instance?.activity?.state === "known" ? instance.activity.value === "working" : false;
    this.emit({
      modelPending: {
        ...this.state.modelPending,
        [instanceId]: { id: model, queued: busy, at: Date.now() },
      },
    });
    try {
      await this.configure(instanceId, this.permissionModeOf(instanceId), { model });
    } catch (error) {
      // A post the Hub/Node refused (offline node, 4xx, network) must be
      // pixel-distinguishable from a switch that worked: clear pending,
      // revert the optimistic selection to the last observed id, and toast
      // the reason — same shape as a model-degraded lifecycle rejection.
      const pending = { ...this.state.modelPending };
      delete pending[instanceId];
      const effective = this.state.modelEffective[instanceId];
      const models = { ...this.state.models };
      if (effective) models[instanceId] = effective.id;
      else delete models[instanceId];
      const reason = error instanceof Error ? error.message : String(error);
      this.toast(`模型切换失败：${reason}`);
      this.emit({ modelPending: pending, models });
      // Swallow after reporting: SessionPage hands this promise straight to
      // the Composer (no void), and the toast is the failure mouth. Re-
      // throwing would only create an unhandled rejection.
    }
  }

  /** §9.1: pending model switch, if any (stale entries expire). */
  modelPendingOf(instanceId: Id): ModelPending | null {
    const pending = this.state.modelPending[instanceId];
    if (pending && Date.now() - pending.at > EFFORT_PENDING_MAX_AGE_MS) {
      const next = { ...this.state.modelPending };
      delete next[instanceId];
      this.state = { ...this.state, modelPending: next };
      return null;
    }
    return pending ?? null;
  }

  /** §9.1: transcript-read-back effective model id, or null when unobserved. */
  modelEffectiveOf(instanceId: Id): ModelEffectiveView | null {
    return this.state.modelEffective[instanceId] ?? null;
  }

  /** §9.1: the session's discovered switchable model list, or null. */
  modelCatalogOf(instanceId: Id): ModelCatalogView | null {
    return this.state.modelCatalogs[instanceId] ?? null;
  }

  /** Runtime permission-mode switch (Claude shift+tab wheel). Optimistic
   *  pending, settled by the `permission` observation; queued while working. */
  async setPermission(instanceId: Id, mode: string) {
    const instance = this.state.instances.find((row) => row.id === instanceId);
    const busy =
      instance?.activity?.state === "known" ? instance.activity.value === "working" : false;
    this.emit({
      permissionMode: { ...this.state.permissionMode, [instanceId]: mode },
      permissionPending: {
        ...this.state.permissionPending,
        [instanceId]: { mode, queued: busy, at: Date.now() },
      },
    });
    try {
      // Configure with permission only (no effort/model): the Node forwards it
      // as a ModelSwitch carrying permissionMode, which the PTY driver turns
      // into a shift+tab wheel walk.
      await api.instanceConfigure(instanceId, this.permissionModeOf(instanceId), {
        permissionMode: mode,
      });
    } catch (error) {
      const pending = { ...this.state.permissionPending };
      delete pending[instanceId];
      this.emit({ permissionPending: pending });
      throw error;
    }
  }

  /** Pending permission walk for an instance, if any. */
  permissionPendingOf(instanceId: Id): PermissionPending | null {
    const pending = this.state.permissionPending[instanceId];
    if (pending && Date.now() - pending.at > PERMISSION_PENDING_MAX_AGE_MS) {
      const next = { ...this.state.permissionPending };
      delete next[instanceId];
      this.state = { ...this.state, permissionPending: next };
      return null;
    }
    return pending ?? null;
  }

  /** Last read-back effective mode for an instance. */
  permissionEffectiveOf(instanceId: Id): PermissionEffectiveView | null {
    return this.state.permissionEffective[instanceId] ?? null;
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
    // The live list projects the phrase from the journal tail into state;
    // the mock client still supplies its scripted summaries directly.
    return this.state.summaries[instanceId] ?? api.summaryOf(instanceId);
  }

  permissionModeOf(instanceId: Id) {
    return this.state.permissionMode[instanceId] ?? api.permissionModeOf(instanceId);
  }

  /** The mode the session launched with (persisted create spec), used to
   *  decide whether bypass is live-reachable in the wheel. */
  launchPermissionModeOf(instanceId: Id): string {
    const instance = this.state.instances.find((row) => row.id === instanceId);
    return normalizeKindPermissionMode(instance?.kind ?? "claude", api.permissionModeOf(instanceId));
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

  /** context-usage-1: Hub-computed per-session usage rollup; null until the
   *  harness has reported at least one usage observation. */
  usageRollupOf(instanceId: Id): UsageRollup | null {
    return (
      this.state.usageRollup[instanceId] ??
      this.state.instances.find((row) => row.id === instanceId)?.usageRollup ??
      null
    );
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
    // The *picker* selection: optimistic launch, a configure push-down, or a
    // terminal `/model` folded in by `noteModelObservation`. It tracks what
    // the picker is sitting on, NOT what was requested of the launch — use
    // `modelRequestedOf` for the requested-vs-running display.
    return (
      this.state.models[instanceId] ??
      instance?.model ??
      (kind === "codex" ? "gpt-5" : kind === "grok" ? "grok-4" : "opus")
    );
  }

  /** The requested model for the requested-vs-running pair (model-pin-1 §5):
   *  an in-flight switch's optimistic id, otherwise the durable launch spec.
   *  It deliberately never reads `state.models` — the picker fold follows a
   *  terminal-side `/model` (and history replay), so after the launch
   *  read-back it would equal the running id and hide the very difference the
   *  pair exists to show. The Hub never overwrites `instance.model` on a
   *  model projection, so it stays the dispatch value. */
  modelRequestedOf(instanceId: Id, kind?: string): string {
    const pending = this.modelPendingOf(instanceId);
    if (pending) return pending.id;
    const instance = this.state.instances.find((row) => row.id === instanceId);
    return (
      instance?.model ??
      (kind === "codex" ? "gpt-5" : kind === "grok" ? "grok-4" : "opus")
    );
  }

  /** The real model list for the picker: discovered catalog ids plus the
   *  current selection; null when nothing has been discovered yet (the slider
   *  falls back to its built-in aliases). */
  modelListOf(instanceId: Id): string[] | null {
    const catalog = this.state.modelCatalogs[instanceId];
    if (!catalog) return null;
    const current = this.modelOf(instanceId);
    const out: string[] = [];
    for (const id of [...catalog.models, current]) {
      if (id && !out.includes(id)) out.push(id);
    }
    return out;
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
    // C2: the Node joins the prompt's hook/transcript evidence onto the
    // delivering command, so the journal user observation carries the same
    // commandId. That is the authoritative settlement and works when two
    // sends have identical text (a text comparison could not tell them
    // apart).
    if (bubble.commandId) {
      const matched = events.some(
        (ev) =>
          ev.kind === "message" &&
          (ev.payload as { commandId?: Id }).commandId === bubble.commandId,
      );
      return matched ? { ...bubble, state: "settled" as const } : bubble;
    }
    // No server id — pre-C2 producers / the in-browser mock append no
    // commandId, so keep the old text rule for them. It only settles the
    // bubble (hide the optimistic copy); it never attributes anything, and a
    // bubble whose POST returned a server id is never settled on text alone.
    const matchedByText = events.some(
      (ev) =>
        ev.kind === "message" &&
        observationText(ev) === bubble.text &&
        (ev.payload as { role?: string }).role === "user",
    );
    return matchedByText ? { ...bubble, state: "settled" as const } : bubble;
  });
}

export const hubStore = new HubStore();

export function useHub(): HubState {
  return useSyncExternalStore(hubStore.subscribe, hubStore.getSnapshot, hubStore.getSnapshot);
}
