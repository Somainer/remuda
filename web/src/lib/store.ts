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
import { doneFromLines, lastLines, latestScreenSnapshot } from "./screen";
import { ConnectionMachine, type ConnectionState, LIVE_FRAME_MS } from "./connection";
import {
  newCommandId,
  openOutboxStorage,
  Outbox,
  withinRetryWindow,
  LEASE_TTL_MS,
  isDeliverable as isDeliverableOutbox,
  type OutboxRecord,
  type OutboxState,
} from "./outbox";

/** Bound on per-row flush passes in one flushAllOutbox call. */
const MAX_FLUSH_PASSES = 100;

/** Bounded reconciliation of a forwarded-but-reconciling command (GET loop). */
const RECONCILE_GET_DEADLINE_MS = 30_000;
const RECONCILE_GET_INTERVAL_MS = 1_000;
/** Bounded backoff for Hub-held (Node offline) same-id re-POSTs. */
const HELD_RETRY_BASE_MS = 2_000;
const HELD_RETRY_MAX_MS = 30_000;
import { liveSummary } from "../features/session/liveSummary";
import { HubHttpError, isUnauthorized } from "./httpError";
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

/**
 * D-055 four-state link (hub-resilience §5.2). `reconnecting` is retained as
 * the journal-completeness label used by SessionPage's merged indicator; the
 * connection machine itself reports live/stale/offline/recovering.
 */
export type ConnectionUi = "live" | "stale" | "offline" | "recovering" | "reconnecting";
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
  /**
   * D-055 durable outbox state, present when this bubble is backed by an
   * outbox row. commandStatus reads it (with the connection state) to show
   * 待发送（离线）/ 发送中 / 未送达 instead of the old unconfirmed fallback.
   */
  outboxState?: OutboxState;
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

/**
 * A rendered screen. `journalSeq` is the ordering basis: for a
 * journal-derived screen it is the seq of the source observation; for a live
 * `tty.screen` RPC result it is the journal seq that buffer was known fresh
 * THROUGH when the read started (null before any journal screen). One total
 * order then holds in both directions: a journal screen whose seq is at or
 * behind the committed basis cannot roll an RPC buffer back (catch-up
 * re-derives the same screen on every later non-screen event), and an RPC
 * read whose basis is behind a journal frame committed during its flight
 * cannot overwrite that frame.
 */
export type ScreenEntry = { lines: string[]; done: boolean; journalSeq: string | null };

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
  screens: Record<string, ScreenEntry>;
  /** List-row live phrases projected from each instance's journal tail. */
  summaries: Record<string, string>;
  /** Number of outbox rows pending/inflight (offline banner). */
  outboxPending: number;
};

const initial: HubState = {
  ready: false,
  authed: false,
  error: null,
  toast: null,
  connection: "recovering",
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
  /** Outbox rows still awaiting a definite Hub answer (D-055 banner count). */
  outboxPending: 0,
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

/**
 * An HTTP poll started before a followed turn boundary cannot undo it, and
 * an optimistically-inserted (just-created) instance cannot be dropped by a
 * list response whose request began before the create completed. `pins`
 * records created ids keyed by the list-request sequence they were created
 * against; `reqSeq` is THIS response's request sequence, and `outstanding`
 * the sequences still in flight (this request included). A pinned id missing
 * from the incoming list is retained while any request at/before its pin is
 * unresolved — including a newer response that lands while an older one is
 * still pending.
 */
function mergeInstanceSnapshots(
  incoming: Instance[],
  current: Instance[],
  pins?: ReadonlyMap<Id, { seq: number; confirmedByNewer: boolean }>,
  reqSeq = Number.POSITIVE_INFINITY,
  outstanding?: ReadonlySet<number>,
): Instance[] {
  const previous = new Map(current.map((instance) => [instance.id, instance]));
  const merged = incoming.map((instance) => {
    const newer = previous.get(instance.id);
    return newer && BigInt(newer.durableSeq) > BigInt(instance.durableSeq) ? newer : instance;
  });
  if (!pins?.size) return merged;
  const seen = new Set(merged.map((instance) => instance.id));
  for (const [id, pin] of pins) {
    if (seen.has(id)) continue;
    const optimistic = previous.get(id);
    if (!optimistic) continue;
    // This response (in `outstanding`), or an even older request still
    // pending, may predate the create: keep the optimistic row. A response
    // that started strictly after the create AND has no older in-flight
    // sibling is authoritative.
    if (Array.from(outstanding ?? [reqSeq]).some((seq) => seq <= pin.seq)) {
      merged.unshift(optimistic);
    }
  }
  return merged;
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
  /**
   * List-fetch sequencing: every refresh() takes a monotonically increasing
   * seq at fetch START. Optimistically created instances are pinned against
   * the seq at create time, and a list response only drops a pinned id once
   * every request at/before that seq has resolved and a newer response
   * confirmed the list — a poll that began before the create can never erase
   * the row when it lands late (the 会话不存在 race after navigation).
   */
  private listReqSeq = 0;
  private listOutstanding = new Set<number>();
  private pinnedCreates = new Map<Id, { seq: number; confirmedByNewer: boolean }>();
  private pollTimer: ReturnType<typeof setInterval> | null = null;
  /**
   * Per-instance monotonic token for `tty.screen` reads: a stale response
   * (a newer read already started) never commits.
   */
  private screenReadGen = new Map<Id, number>();
  /**
   * D-055 auto-reconnect: one connection machine for the active follow
   * socket, plus the durable outbox and the instance it is bound to. Both are
   * created lazily at bootstrap so non-authenticated and unit-test callers
   * don't touch IndexedDB.
   */
  private connection: ConnectionMachine | null = null;
  private outbox: Outbox | null = null;
  private outboxDegraded = false;
  private connectionBoundTo: Id | null = null;
  private connectionBoundJournal: Id | null = null;
  private outboxInit: Promise<boolean> | null = null;

  /**
   * Lazily open the durable outbox (also used outside bootstrap, e.g. a
   * send that races first mount or unit-test callers). Resolves false when no
   * storage backend is writable — send then uses the legacy direct path.
   */
  private ensureOutbox(): Promise<boolean> {
    if (this.outbox) return Promise.resolve(true);
    if (this.outboxInit) return this.outboxInit;
    this.outboxInit = (async () => {
      try {
        const opened = await openOutboxStorage();
        this.outboxDegraded = opened.degraded;
        const box = await Outbox.load(opened.storage);
        this.outbox = box;
        box.subscribe(() => this.emit({ outboxPending: box.pendingCount() }));
        this.emit({ outboxPending: box.pendingCount() });
        if (opened.degraded) this.toast("本机无法可靠保存待发消息（存储不可用）");
        // Rows left inflight by a closed tab are due immediately.
        void this.flushAllOutbox();
        return true;
      } catch {
        this.outboxDegraded = true;
        return false;
      }
    })();
    return this.outboxInit;
  }
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

  /** Apply one transcript-read-back model observation. `live` events settle a
   *  pending push-down and fold the effective id into the PICKER selection
   *  (a terminal `/model` moves the picker without a configure round-trip);
   *  history replay only hydrates effective/catalog state. The chip shows the
   *  effective id itself; the client does not reconstruct a "requested" model
   *  (model-pin-1 §5.4). */
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
      // Our own push-down settled: clear pending, park the picker on the
      // resolved id.
      patch.modelPending = { ...this.state.modelPending };
      delete patch.modelPending[instanceId];
      patch.models = { ...this.state.models, [instanceId]: parsed.effective.id };
    } else if (live && !catalogOnly) {
      // Live model edge (launch read-back or a terminal-side switch): fold
      // the effective id into the picker selection.
      patch.models = { ...this.state.models, [instanceId]: parsed.effective.id };
    } else if (!live && this.state.models[instanceId] == null) {
      // History replay on a fresh mount: seed the picker from the observed id
      // so it reflects the resolved model after reload.
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

  /**
   * D-055: create the outbox + connection machine once per authenticated
   * session, restore unsent rows as optimistic bubbles (survives reload), and
   * drive the machine from browser reachability events.
   *
   * @param offline  when true (bootstrap hello failed with a device session
   * still present — i.e. the Hub is unreachable on reload) the machine starts
   * at offline instead of an optimistic live, so cached UI shows an honest
   * state and the outbox still restores/flushes on recovery.
   */
  private async initConnection(offline = false) {
    if (this.connection) return;
    await this.ensureOutbox();
    const box = this.outbox;
    if (!box) return;

    // Reload: restore unsent rows as bubbles, keyed to their stored instances.
    const restored = box.unresolved();
    if (restored.length) {
      // Offline reload never ran the instance list. Restore the LAST KNOWN
      // complete instance projection persisted with each row (no fabricated
      // stubs/casts); SessionPage renders the real shape with its capabilities
      // and nativeRef intact. The next successful list refresh replaces these.
      const restoredInstances: Instance[] = [];
      for (const r of restored) {
        if (this.state.instances.some((i) => i.id === r.instanceId)) continue;
        const known =
          r.instanceSnapshot ?? restoredInstances.find((i) => i.id === r.instanceId);
        if (known) restoredInstances.push(known);
      }
      this.emit({
        instances: [...this.state.instances, ...restoredInstances],
        bubbles: [
          ...restored.map((r) => this.bubbleFromOutbox(r)),
          ...this.state.bubbles,
        ],
      });
    }

    const machine = new ConnectionMachine({
      resume: () => this.resumeConnection(),
      probe: () => this.connectionProbe(),
      isFollowLive: () => this.followSocketLive(),
      onState: (state) => {
        this.emit({ connection: state, outboxPending: box.pendingCount() });
        // A socket-level recovery is also a moment to deliver the queue.
        if (state === "live") void this.flushAllOutbox();
      },
    });
    this.connection = machine;
    // Honest bootstrap: do NOT optimistically publish live. The machine waits
    // for the follow socket to open AND frame (or a successful resume) before
    // live; only a genuinely offline bootstrap (no device session after a
    // network error) starts the offline retry loop.
    if (offline) machine.setStateOffline();
    else machine.markRecovering();

    if (typeof window !== "undefined") {
      window.addEventListener("online", this.onConnOnline);
      window.addEventListener("offline", this.onConnOffline);
      window.addEventListener("pageshow", this.onConnPageshow);
      document.addEventListener("visibilitychange", this.onConnVisibility);
      window.addEventListener("focus", this.onConnFocus);
      window.addEventListener("pagehide", this.onPageHide);
      window.addEventListener("beforeunload", this.onPageHide);
    }

    // Send anything already due (e.g. rows left inflight when the tab closed).
    void this.flushAllOutbox();
  }

  private onConnOnline = () => this.connection?.dispatch({ type: "online" });
  private onConnOffline = () => this.connection?.dispatch({ type: "offline" });
  private onConnPageshow = (event: PageTransitionEvent) => {
    // BFCache restore (iOS app switch, Back): the earlier pagehide set the
    // unload flag, but the page is alive again — clear it so flushes resume.
    // The flag must never outlive the unload that set it.
    this.pageIsUnloading = false;
    if (event.persisted) {
      this.connection?.dispatch({ type: "resume" });
      // Resume any rows queued while the page was suspended.
      void this.flushAllOutbox();
    }
  };
  private onConnVisibility = () => {
    if (document.visibilityState === "visible") {
      // Returning from an iOS backgrounding fires visibilitychange without a
      // persisted pageshow; treat it as the same unload-flag reset.
      this.pageIsUnloading = false;
    }
    this.connection?.setVisibility(document.visibilityState === "visible");
  };
  private focusLast = 0;
  private onConnFocus = () => {
    // Throttle the catch-all focus trigger to 5 s.
    const t = Date.now();
    if (t - this.focusLast < 5_000) return;
    this.focusLast = t;
    this.connection?.dispatch({ type: "resume" });
  };

  /** Link state the UI gates writes on (D-055). Optimistically live until the
   * connection machine reports otherwise (it only exists post-bootstrap). */
  get connectionState(): ConnectionState {
    return (this.connection?.state ?? this.connectionStateOverride ?? "live") as ConnectionState;
  }

  /**
   * Interrupt is time-sensitive and non-idempotent across turns. Refused only
   * when there is no working link (offline) or a resume/catch-up is in flight
   * (recovering). A live-but-quiet (stale) session can still be interrupted —
   * the cancel POST itself proves the path works.
   */
  get canInterrupt(): boolean {
    return this.connectionState !== "offline" && this.connectionState !== "recovering";
  }

  /** readyState of the current follow socket (1 = OPEN). */
  private followReadyState = -1;
  /** Epoch ms of the last frame (event/tick/snapshot) on the current socket. */
  private lastFollowFrameAt = 0;

  /**
   * The follow link is genuinely live only when the socket is OPEN AND a frame
   * arrived within LIVE_FRAME_MS. This is the ONLY thing that lets a
   * foreground resume trust a cached "live" instead of reopening.
   */
  private followSocketLive(): boolean {
    if (this.followReadyState !== 1) return false;
    if (!this.lastFollowFrameAt) return false;
    return Date.now() - this.lastFollowFrameAt <= LIVE_FRAME_MS;
  }

  /** Test-only: force the follow-live verdict (simulates open + fresh frame). */
  setFollowLiveForTest(open: boolean, framed: boolean) {
    this.followReadyState = open ? 1 : -1;
    this.lastFollowFrameAt = framed ? Date.now() : 0;
  }

  /** Test-only: drive the machine live the way a fresh follow frame would. */
  frameForTest() {
    this.setFollowLiveForTest(true, true);
    this.connection?.dispatch({ type: "frame" });
  }

  /** Test-only: override the optimistic-live link state without a machine. */
  setConnectionStateForTest(state: ConnectionState) {
    this.emit({ connection: state, outboxPending: this.outbox?.pendingCount() ?? 0 });
    this.connectionStateOverride = state;
  }

  /** Test-only: drive the pagehide/pageshow lifecycle (BFCache, iOS). */
  async pageShowForTest(persisted: boolean): Promise<void> {
    this.onPageHide();
    this.onConnPageshow({ persisted } as PageTransitionEvent);
    // Let the resumed flush microtasks run.
    await Promise.resolve();
    await Promise.resolve();
  }

  get pageIsUnloadingForTest(): boolean {
    return this.pageIsUnloading;
  }
  private connectionStateOverride: ConnectionState | null = null;
  /** True during pagehide/beforeunload: no new outbox POSTs. */
  private pageIsUnloading = false;
  /** Auto-clear for a beforeunload prompt the user CANCELS (page stays). */
  private unloadClearTimer: ReturnType<typeof setTimeout> | null = null;
  private onPageHide = () => {
    this.pageIsUnloading = true;
    // Stop any pending live-retry timer so it cannot POST during teardown.
    for (const [, t] of this.outboxRetryTimer) clearTimeout(t);
    this.outboxRetryTimer.clear();
    // beforeunload can be CANCELLED (the user stays on the page); pagehide
    // only fires when navigation actually proceeds. A beforeunload that does
    // not lead to a real unload must not latch delivery off forever: reset
    // the flag on the next task, unless a genuine pageshow/pagehide sequence
    // re-asserts it.
    if (this.unloadClearTimer) clearTimeout(this.unloadClearTimer);
    this.unloadClearTimer = setTimeout(() => {
      this.pageIsUnloading = false;
      this.unloadClearTimer = null;
    }, 0);
  };

  get outboxUnavailable(): boolean {
    return this.outboxDegraded;
  }

  private async connectionProbe(): Promise<boolean> {
    const journalId = this.connectionBoundJournal;
    if (!journalId) return true;
    try {
      await api.eventsRead({ journalId, limit: 1 });
      return true;
    } catch {
      return false;
    }
  }

  /**
   * The machine's resume action. It certifies live ONLY when the follow socket
   * reopened AND the bounded journal catch-up succeeded — REST reachability
   * alone is not enough (a dead follow stream with working HTTP otherwise
   * leaves the transcript frozen under a false live). Outbox delivery runs
   * first (a POST proves the path) but its outcome never masks a socket
   * failure: a "held" (host offline) row is a legitimate non-terminal result,
   * while a follow/catch-up failure throws so the machine stays offline and
   * retries.
   */
  private async resumeConnection() {
    await this.flushAllOutbox();
    if (this.connectionBoundTo) {
      // Throws on socket-open or catch-up failure → machine remains offline
      // and retries; no toast-swallowing into false live.
      await this.reopenFollow(this.connectionBoundTo);
    }
    await Promise.all([this.refresh().catch(() => undefined), this.refreshHosts().catch(() => undefined)]);
  }

  /** Reopen the follow socket for an already-mounted instance and resync. */
  private async reopenFollow(instanceId: Id) {
    const instance =
      this.state.instances.find((i) => i.id === instanceId) ?? (await api.instanceGet(instanceId));
    const client = this.journals.get(instance.journalId);
    // Chain on any in-flight reconciliation so an older screen/resync job
    // cannot land after this fresher one; errors propagate to the machine.
    await this.chainReconcile(instanceId, async () => {
      if (client) {
        await this.openFollowSocket(instance, client, client.appliedSeq);
        await client.resumeAfterReconnect();
      } else {
        await this.follow(instanceId);
      }
    });
  }

  /** Foreground trigger used by Shell/PhoneShell (replaces raw catchup). */
  resumeActive(instanceId: Id | null) {
    if (instanceId) {
      this.connectionBoundTo = instanceId;
      const j = this.state.instances.find((i) => i.id === instanceId)?.journalId;
      this.connectionBoundJournal = j ?? null;
    }
    this.connection?.dispatch({ type: "resume" });
    if (!this.connection && instanceId) void this.catchup(instanceId);
  }

  /**
   * Flush every instance with pending rows. Each ROW is delivered under its own
   * lock acquisition (rather than one lock for the whole instance) so a steer
   * of a later row queued while an earlier POST is in flight interleaves at the
   * lock boundary and promotes that row before it is POSTed. Still a single
   * deliverer: the cross-tab/durable lock means two flushes never run a row
   * concurrently.
   *
   * `attempted` is the per-FLUSH set of rows that already got a POST: a row
   * answered "held" (Node offline) must NOT be hammered again on the next pass
   * of the SAME flush — its bounded retry timer / the next reconnect flush
   * re-forwards it. Without this set the drain loop would tight-loop a held
   * row up to MAX_FLUSH_PASSES in one flush. A genuinely new row (or a steer
   * landing mid-flush) is not in the set and still drains.
   */
  private async flushAllOutbox(attempted: Set<Id> = new Set()) {
    const box = this.outbox;
    if (!box) return;
    if (this.pageIsUnloading) return;
    // Drain passes; each pass acquires the lock, posts at most the FIRST
    // deliverable unattempted row, and releases — letting queued conversions
    // interleave.
    for (let pass = 0; pass < MAX_FLUSH_PASSES; pass += 1) {
      if (this.pageIsUnloading) return;
      const instances = [...new Set(box.pending().map((r) => r.instanceId))];
      let deliveredNew = false;
      for (const instanceId of instances) {
        if (this.pageIsUnloading) return;
        try {
          const id = await box.withInstanceLock(instanceId, (iid, deliverable) =>
            this.deliverOneRow(iid, deliverable.filter((r) => !attempted.has(r.commandId))),
          );
          if (id) {
            attempted.add(id);
            deliveredNew = true;
          }
        } catch {
          // The lease/lock acquisition failed (never a double POST: the row is
          // inflight with a fresh durable lease). A retry timer / reconnect
          // re-triggers under the same id; keep draining other instances.
        }
      }
      if (!deliveredNew) break;
    }
  }

  /**
   * Deliver ONE authoritative row (the first fresh one), re-reading its mode
   * from the box so a just-landed steer promotion takes effect. Runs inside the
   * single-deliverer lock. Returns the POSTed row's commandId (added to the
   * flush's attempted set) or null when nothing was POSTed.
   */
  private async deliverOneRow(instanceId: Id, deliverable: OutboxRecord[]): Promise<Id | null> {
    const box = this.outbox;
    if (!box) return null;
    const rec = deliverable[0];
    if (!rec) return null;
    const fresh = box.get(rec.commandId) ?? rec;
    if (!isDeliverableOutbox(fresh, Date.now())) return null;
    const attemptedPost = await this.deliverOutboxRecord(fresh);
    if (!attemptedPost) return null;
    // If that drained the instance, run the post-delivery resync/screen chain
    // exactly once.
    if (!box.pendingFor(instanceId).length) {
      await this.chainReconcile(instanceId, async () => {
        const client = this.journals.get(
          this.state.instances.find((i) => i.id === instanceId)?.journalId ?? "",
        );
        try {
          await client?.resumeAfterReconnect();
        } catch {
          /* machine owns the failure */
        }
        const events = this.state.events[instanceId] ?? [];
        this.settleFromJournal(instanceId, events);
        try {
          await this.refreshScreen(instanceId);
        } catch (err) {
          if (!isScreenNodeBusy(err)) this.reconcileToast(err, "屏幕同步");
        }
      });
    }
    return fresh.commandId;
  }

  /**
   * Classify a Hub command record into an outbox outcome.
   *  - rejected: settled with settlement.outcome rejected (real Node reject).
   *  - held: queued and never forwarded (Node offline); same-id re-POST later.
   *  - reconciling: forwarded but the Hub's resolution is still "reconciling";
   *    settled by a bounded GET (never re-forwarded).
   *  - sent: accepted/settled-completed or a clear forwarded row; reached the
   *    Hub/Node, await the journal join (never re-POST).
   */
  private classifyCommandResult(command: Command): "sent" | "held" | "reconciling" | "rejected" {
    if (command.state === "settled" && command.settlement?.outcome === "rejected") {
      return "rejected";
    }
    if (
      command.state === "queued" &&
      command.dispatch !== "transport-written" &&
      command.dispatch !== "native-acknowledged"
    ) {
      return "held";
    }
    if (command.state === "queued" && command.resolution === "reconciling") {
      return "reconciling";
    }
    return "sent";
  }

  private async reconcileCommandViaGet(
    instanceId: Id,
    commandId: Id,
  ): Promise<"sent" | "held" | "reconciling" | "rejected" | null> {
    try {
      const result = await api.instanceCommandStatus(instanceId, commandId);
      return this.classifyCommandResult(result.command);
    } catch {
      return null;
    }
  }

  /**
   * Bounded reconciliation of a "reconciling" row: poll the GET endpoint until
   * it is accepted/settled (→ sent), rejected, or the deadline passes (→
   * unknown). NEVER re-POSTs — the Hub already forwarded the command. Runs
   * OUTSIDE the single-deliverer lock (it can take the whole 30 s deadline and
   * must not block another row of the instance, e.g. a steer); a reconciling
   * row is non-deliverable, so no other owner can POST it meanwhile.
   */
  private reconcileReconcilingRow(instanceId: Id, commandId: Id): void {
    void this.runReconcileReconcilingRow(instanceId, commandId);
  }

  private async runReconcileReconcilingRow(instanceId: Id, commandId: Id): Promise<void> {
    const deadline = Date.now() + RECONCILE_GET_DEADLINE_MS;
    for (;;) {
      const verdict = await this.reconcileCommandViaGet(instanceId, commandId);
      if (verdict === "rejected") {
        const result = await api.instanceCommandStatus(instanceId, commandId).catch(() => null);
        await this.safePatch(commandId, {
          state: "rejected",
          serverState: result?.command.state,
          gotResponse: true,
          lastError: result?.command.settlement?.reason ?? "rejected by node",
        });
        return;
      }
      if (verdict === "sent") {
        await this.safePatch(commandId, { state: "sent", gotResponse: true });
        return;
      }
      if (verdict === "held") {
        // Host went offline mid-reconcile: fall back to held retry (refund the
        // attempt and arm the bounded same-id re-POST).
        await this.safePatch(commandId, {
          state: "held",
          gotResponse: true,
          attempts: this.outbox?.get(commandId)?.attempts ?? 0,
        });
        this.scheduleHeldRetry(instanceId);
        return;
      }
      if (Date.now() >= deadline) {
        await this.safePatch(commandId, { state: "unknown", lastError: "reconciliation deadline" });
        return;
      }
      await new Promise((r) => setTimeout(r, RECONCILE_GET_INTERVAL_MS));
    }
  }

  /** Storage-first patch that never throws into the delivery flow. */
  private async safePatch(commandId: Id, patch: Partial<OutboxRecord>) {
    try {
      await this.outbox?.patch(commandId, patch);
    } catch {
      /* storage failure: the durable inflight/lease state is the recovery path */
    }
    this.syncBubbleFromOutbox(commandId);
  }

  /**
   * Deliver ONE authoritative durable row (handed in by the single-deliverer
   * lock). Claims a durable inflight lease before POSTing; a storage abort on
   * that claim means no POST and the row stays deliverable. "held" retries do
   * not spend the attempt budget; a queued-forwarded reconciling row is
   * settled by GET under a bounded deadline. Returns true when a POST ran.
   */
  private async deliverOutboxRecord(current0: OutboxRecord): Promise<boolean> {
    const box = this.outbox;
    if (!box) return false;
    const commandId = current0.commandId;
    if (!withinRetryWindow(current0)) {
      await this.safePatch(commandId, { state: "unknown", lastError: "retry window exhausted" });
      return false;
    }

    // Claim the durable inflight lease storage-first. If this write aborts,
    // never POST: the cache/storage still show a deliverable row (the cache
    // updates only after commit), so the next trigger retries under the same
    // commandId. Another owner is excluded by the durable lease, not an
    // in-memory marker.
    const isFreshAttempt = current0.state !== "held";
    try {
      await box.patch(commandId, {
        state: "inflight",
        attempts: isFreshAttempt ? current0.attempts + 1 : current0.attempts,
        lease: { owner: box.ownerId, until: Date.now() + LEASE_TTL_MS },
      });
    } catch {
      return false;
    }
    this.syncBubbleFromOutbox(commandId);

    const finish = async (patch: Partial<OutboxRecord>) => {
      await this.safePatch(commandId, { ...patch, lease: undefined });
    };

    try {
      const result = await api.instanceSend(
        current0.instanceId,
        current0.prompt,
        current0.attachments ?? [],
        current0.mode,
        commandId,
      );
      const classified = this.classifyCommandResult(result.command);
      if (classified === "rejected") {
        await finish({
          state: "rejected",
          serverState: result.command.state,
          gotResponse: true,
          lastError: result.command.settlement?.reason ?? "rejected by node",
        });
      } else if (classified === "held") {
        // Node offline: Hub holds the row; bounded same-id retry, no attempt
        // spent (the 20-attempt budget must not drain while the host is down).
        // A host online→online resume (flushAllOutbox) re-POSTs.
        await finish({
          state: "held",
          serverState: result.command.state,
          gotResponse: true,
          attempts: current0.attempts,
        });
        this.scheduleHeldRetry(current0.instanceId);
      } else if (classified === "reconciling") {
        await finish({ state: "reconciling", serverState: result.command.state, gotResponse: true });
        await this.reconcileReconcilingRow(current0.instanceId, commandId);
      } else {
        await finish({ state: "sent", serverState: result.command.state, gotResponse: true });
      }
    } catch (err) {
      const networkFailure =
        !(err instanceof HubHttpError) || err.status >= 500 || err.status === 503;
      if (networkFailure) {
        const reconciled = await this.reconcileCommandViaGet(current0.instanceId, commandId);
        if (reconciled === "rejected") {
          await finish({ state: "rejected", gotResponse: true, lastError: "rejected by node" });
        } else if (reconciled === "sent") {
          await finish({ state: "sent", gotResponse: true });
        } else if (reconciled === "held") {
          await finish({ state: "held", gotResponse: true, attempts: current0.attempts });
          this.scheduleHeldRetry(current0.instanceId);
        } else if (reconciled === "reconciling") {
          await finish({ state: "reconciling", gotResponse: true });
          await this.reconcileReconcilingRow(current0.instanceId, commandId);
        } else {
          // Truly undelivered: keep pending, bounded same-id retry. Same-id
          // retries are safe in ANY link state (the 20-attempt / 24 h envelope
          // bounds them), and a reconnect flush also drains the row.
          await this.safePatch(commandId, { state: "pending", lease: undefined, lastError: String(err) });
          this.scheduleOutboxRetry(current0.instanceId);
        }
      } else if (err instanceof HubHttpError && err.status === 409) {
        await finish({ state: "unknown", lastError: err.message });
      } else if (err instanceof HubHttpError) {
        await finish({ state: "rejected", lastError: err.message });
      }
    }
    this.syncBubbleFromOutbox(commandId);
    if (!box.pendingFor(current0.instanceId).length) this.clearOutboxRetry(current0.instanceId);
    return true;
  }

  private bubbleFromOutbox(r: OutboxRecord): LocalBubble {
    // Merge onto any live bubble for the same clientRequestId so transient
    // preview data (blob previewUrl, present only in the live bubble, not
    // persisted) survives a state sync. On a fresh restore only the persisted
    // manifest refs are available and the UI links straight to the Hub object.
    const live = this.state.bubbles.find((b) => b.clientRequestId === r.clientRequestId);
    const attachments = live?.attachments?.length
      ? live.attachments
      : ((r.attachments ?? []) as BubbleAttachment[]);
    return {
      clientRequestId: r.clientRequestId,
      instanceId: r.instanceId,
      text: r.prompt,
      // The client-generated id is the wire commandId from the first POST.
      commandId: r.commandId,
      held: false,
      state:
        r.state === "done"
          ? "settled"
          : r.state === "rejected" || r.state === "unknown"
            ? "unknown"
            : // pending/inflight = queued; "sent"/"held" already reached the
              // Hub and render as an accepted (delivered, not 待确认) row.
              r.state === "pending" || r.state === "inflight"
              ? "queued"
              : "accepted",
      outboxState: r.state,
      ...(attachments.length ? { attachments } : {}),
      promptMode: r.mode ?? "new-turn",
      createdAt: new Date(r.createdAt).toISOString(),
    };
  }

  private syncBubbleFromOutbox(commandId: Id) {
    const rec = this.outbox?.get(commandId);
    if (!rec) return;
    const next = this.bubbleFromOutbox(rec);
    this.emit({
      bubbles: this.state.bubbles.map((b) => (b.clientRequestId === rec.clientRequestId ? next : b)),
    });
  }

  /**
   * Mark bubbles settled by commandId journal evidence AND retire their
   * outbox rows to "done" (a journal message is stronger than a sent/held
   * Hub row). Used on catch-up/resync batches that aren't the live onEvents.
   */
  private settleFromJournal(instanceId: Id, events: Observation[]) {
    const next = settleBubbles(this.state.bubbles, instanceId, events);
    if (this.outbox) {
      for (const ev of events) {
        if (ev.kind !== "message") continue;
        const commandId = (ev.payload as { commandId?: Id }).commandId;
        const rec = commandId ? this.outbox.get(commandId) : null;
        if (commandId && rec && rec.state !== "done" && rec.state !== "rejected" && rec.state !== "unknown") {
          void this.outbox.patch(commandId, { state: "done", serverState: "journal-settled" });
        }
      }
    }
    this.emit({ bubbles: next });
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
        // Link state is the connection machine's to publish, not REST's; leave
        // the current (recovering) value until a follow frame certifies live.
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
      await this.initConnection();
      this.startPoll();
    } catch (err) {
      if (gen !== this.bootGen) return;
      const unauth = isUnauthorized(err);
      if (unauth) {
        clearSession();
        dropDeviceCookie();
      }
      // A network failure WITH a device session still on disk is an offline
      // reload, not a logout: keep the (cached) authed UI, restore the outbox,
      // and let the connection machine reconnect. Only a real 401 logs out.
      const hasSession = api.hasDeviceSession();
      if (!unauth && hasSession) {
        await this.initConnection(true);
        this.emit({
          ready: true,
          authed: true,
          session: readSession(),
          error: null,
        });
        this.startPoll();
        return;
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
    // Invalidate this auth epoch: a list fetch already in flight (the 2 s
    // poll may be awaiting when the user logs out) must be dropped wholesale
    // when it resolves — neither replace the wiped instance list with the old
    // session's rows nor release pins a re-login creates. The request seq
    // stays monotonic ACROSS logouts on purpose: it is captured at fetch
    // start and compared against create pins, so resetting it would let a
    // pre-logout response look newer than a post-login create. Do not clear
    // listOutstanding: the stale request still owns its own entry and removes
    // it in its finally.
    ++this.bootGen;
    this.pinnedCreates.clear();
    // Tear down the connection machine and its browser listeners (init
    // recreates them at the next bootstrap); the durable outbox itself stays.
    this.connection?.dispose();
    this.connection = null;
    this.connectionBoundTo = null;
    this.connectionBoundJournal = null;
    for (const [, t] of this.outboxRetryTimer) clearTimeout(t);
    this.outboxRetryTimer.clear();
    this.outboxRetryAttempt.clear();
    if (typeof window !== "undefined") {
      window.removeEventListener("online", this.onConnOnline);
      window.removeEventListener("offline", this.onConnOffline);
      window.removeEventListener("pageshow", this.onConnPageshow);
      document.removeEventListener("visibilitychange", this.onConnVisibility);
      window.removeEventListener("focus", this.onConnFocus);
      window.removeEventListener("pagehide", this.onPageHide);
      window.removeEventListener("beforeunload", this.onPageHide);
      this.pageIsUnloading = false;
    }
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
    // Sequence this fetch from its START, not its resolution: a create that
    // lands while the request is in flight must survive this response and be
    // released only by a response newer than the create once this one is in.
    const reqSeq = ++this.listReqSeq;
    // Bind the response to the auth epoch captured at fetch START. A logout
    // (or a new bootstrap) while the request is in flight invalidates it: its
    // body belongs to the old session and must not merge or touch pins.
    const epoch = this.bootGen;
    this.listOutstanding.add(reqSeq);
    try {
      const [instances, interactions] = await Promise.all([api.instanceList(), api.interactionList()]);
      if (epoch !== this.bootGen) return;
      this.emit({
        instances: mergeInstanceSnapshots(
          instances.items,
          this.state.instances,
          this.pinnedCreates,
          reqSeq,
          this.listOutstanding,
        ),
        interactions,
      });
      // A response newer than a pin proves the server has spoken after the
      // create. Combined with the in-flight sweep below (every older request
      // answered), that is when dropping the pin on a missing id is safe.
      for (const pin of this.pinnedCreates.values()) {
        if (reqSeq > pin.seq) pin.confirmedByNewer = true;
      }
      this.hydrateEffortEffective(instances.items);
      this.hydrateUsageRollups(instances.items);
      this.hydrateModels(instances.items);
      this.hydratePermissionEffective(instances.items);
      // NOTE: list-row live phrases are NOT derived here. refresh() fans into
      // every authenticated path (close/cancel/create/resume re-enter it) and
      // must not add journal polling; the mounted SessionList hydrates phrases
      // for the rows it renders via hydrateRowSummaries().
    } finally {
      // The stale request always releases its own outstanding slot; it is the
      // epoch guard around the reap below that keeps it from moving pins — no
      // control flow in this finally.
      this.listOutstanding.delete(reqSeq);
      if (epoch === this.bootGen) {
        for (const [id, pin] of this.pinnedCreates) {
          if (!pin.confirmedByNewer) continue;
          let olderInFlight = false;
          for (const seq of this.listOutstanding) {
            if (seq <= pin.seq) {
              olderInFlight = true;
              break;
            }
          }
          if (!olderInFlight) this.pinnedCreates.delete(id);
        }
      }
    }
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
    // Already mounted (SessionPage re-render): rebind the connection machine
    // to this active follow; a dead socket is reopened by the machine.
    if (this.journals.has(instance.journalId)) {
      this.connectionBoundTo = instanceId;
      this.connectionBoundJournal = instance.journalId;
      return;
    }
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
        const screen = latestScreenSnapshot(next);
        const phrase = liveSummary(next);
        // Clear a finished run's phrase so the row falls back to its
        // constant sentence instead of keeping a stale (invented) status.
        const summaries = { ...this.state.summaries };
        if (phrase) summaries[instanceId] = phrase;
        else delete summaries[instanceId];
        // Commit through the one screen ordering: catch-up re-derives the
        // latest journal screen on EVERY batch (including non-screen events),
        // and a live RPC buffer already fresh through that seq is newer — a
        // re-derived equal-or-older seq must not roll it (DONE badge flapping
        // while the new turn works).
        const screenPatch =
          screen.lines.length && screen.seq !== null
            ? this.screenCommitPatch(instanceId, lastLines(screen.lines, 80), doneFromLines(screen.lines), {
                kind: "journal",
                seq: screen.seq,
              })
            : null;
        const settledBubbles = settleBubbles(this.state.bubbles, instanceId, next);
        // A journal message is stronger evidence than the queued/accepted Hub
        // row: retire the matching outbox rows so a later reconnect cannot
        // keep retrying a command that demonstrably executed.
        if (this.outbox) {
          for (const ev of next) {
            if (ev.kind !== "message") continue;
            const commandId = (ev.payload as { commandId?: Id }).commandId;
            const rec = commandId ? this.outbox.get(commandId) : null;
            if (commandId && rec && rec.state !== "done" && rec.state !== "rejected") {
              void this.outbox.patch(commandId, { state: "done", serverState: "journal-settled" });
            }
          }
        }
        this.emit({
          instances: applyInstanceActivity(this.state.instances, events),
          events: { ...this.state.events, [instanceId]: next },
          bubbles: settledBubbles,
          screens: screenPatch ?? this.state.screens,
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
        // Journal completeness is a PER-SESSION concern only; it must never
        // publish the global connection state (only the connection machine
        // publishes live, and a journal live read does not prove the follow
        // socket is open — a frozen transcript could otherwise show 已连接).
        this.emit({ journalStatus: { ...this.state.journalStatus, [instanceId]: status } });
      },
      onGap: (from, to) => {
        void client.fillGap(from, to, client.currentResumeGen()).then((acked) => {
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
    await this.openFollowSocket(instance, client, last, {
      earliestRetainedSeq: seed.windowFromSeq ?? "1",
      complete: seed.reachedAfterSeq,
    });
    // This is the active session socket: bind the connection machine to it.
    this.connectionBoundTo = instance.id;
    this.connectionBoundJournal = instance.journalId;
    // Events landing between the REST seed and the socket open arrive on the
    // follow snapshot (filtered past `last`) and flow through applyBatch, so no
    // second tail read is needed.
  }

  /**
   * Open (or reopen) the follow socket for a mounted journal client. Reused by
   * the initial follow and by the connection machine's resume; eventsSubscribe
   * itself closes any prior socket for the journal first.
   */
  private async openFollowSocket(
    instance: Instance,
    client: JournalClient,
    afterSeq: U64,
    /** REST-seed floor for the FIRST subscribe; null on a reopen (keep the client's). */
    seedFloor: { earliestRetainedSeq: string; complete: boolean } | null = null,
  ) {
    const sub = await api.eventsSubscribe(
      instance.journalId,
      afterSeq,
      (batch) => {
        const result = client.applyBatch(batch);
        if (result.acked) {
          void api.eventsAck(batch.subscriptionId, instance.journalId, result.acked);
          // A contiguous live batch that advanced the cursor supersedes any
          // resume/fill read still in flight (item 9).
          client.noteSocketCaughtUp();
        }
        if (result.gap) void client.fillGap(result.gap.from, result.gap.to, client.currentResumeGen());
      },
      (windowFloor) => void client.fillResyncGap(windowFloor),
      {
        onFrame: () => {
          this.lastFollowFrameAt = Date.now();
          this.connection?.dispatch({ type: "frame" });
        },
        onOpen: () => {
          // readyState is tracked via getReadyState after the promise
          // resolves; onOpen only clears a stale frame clock reset.
        },
        // Genuine remote close of the CURRENT socket (eventsSubscribe ignores
        // the close of a socket it intentionally replaced — see api.ts).
        onClose: () => {
          this.followReadyState = -1;
          this.lastFollowFrameAt = 0;
          this.connection?.dispatch({ type: "close" });
        },
      },
    );
    this.subs.set(instance.journalId, sub.subscriptionId);
    this.followReadyState = sub.getReadyState();
    // The subscribe SNAPSHOT is the reopen + catch-up certificate: the server
    // answered over this exact socket. Count it as a frame so a resume
    // certifies live even for an idle session with no subsequent events.
    if (this.followReadyState === 1) {
      this.lastFollowFrameAt = Date.now();
      this.connection?.dispatch({ type: "frame" });
    }
    if (Number(sub.snapshot.asOfSeq) >= Number(afterSeq)) {
      // An EMPTY follow snapshot only says "no events past the afterSeq
      // cursor" — it says nothing about retention below it. Only the first
      // subscribe (REST seed present) overrides the floor; a reopen keeps the
      // client's existing window floor.
      const seedHistory = seedFloor ?? (sub.windowFromSeq === null ? null : sub.snapshot.history);
      if (seedHistory) client.applySnapshot({ ...sub.snapshot, history: seedHistory });
    }
    const tail = mockGappedTail(instance.journalId);
    if (tail) {
      setTimeout(() => {
        const result = client.applyBatch({ ...tail, subscriptionId: sub.subscriptionId });
        if (result.gap) void client.fillGap(result.gap.from, result.gap.to);
      }, 0);
    }
  }

  /** Fetch one window of older history for the transcript's load-earlier row. */
  async loadEarlier(instanceId: Id) {
    const instance = this.state.instances.find((i) => i.id === instanceId);
    if (!instance) return null;
    const client = this.journals.get(instance.journalId);
    if (!client) return null;
    return client.loadEarlier();
  }

  /**
   * Surface a background reconciliation failure on the existing toast mouth
   * instead of swallowing it. These reads never gate the user action (POST
   * landings resolve without them), so this is advisory; the 2 s poll and the
   * follow socket self-heal the same state.
   */
  private reconcileToast(err: unknown, what: string) {
    const message = err instanceof Error && err.message ? err.message : String(err);
    this.toast(`${what}失败：${message}（将在下次轮询重试）`);
  }

  /**
   * Foreground/manual journal catch-up. With D-055 this drives the connection
   * machine (which owns the real live/stale/offline/recovering state and
   * reopens the socket); the manual fallback below remains for callers before
   * the machine initialised (unit tests, degraded mode).
   */
  async catchup(instanceId: Id) {
    const instance = this.state.instances.find((i) => i.id === instanceId);
    if (!instance) return;
    this.connectionBoundTo = instanceId;
    this.connectionBoundJournal = instance.journalId;
    if (this.connection) {
      // resumeConnection reopens the socket + resyncs + flushes the outbox.
      this.connection.dispatch({ type: "resume" });
      return;
    }
    await this.catchupManual(instanceId);
  }

  private async catchupManual(instanceId: Id) {
    const instance = this.state.instances.find((i) => i.id === instanceId);
    if (!instance) return;
    const client = this.journals.get(instance.journalId);
    if (!client) {
      // follow() seeds its own connection state; only surface its failure.
      try {
        await this.follow(instanceId);
      } catch (err) {
        this.reconcileToast(err, "会话同步");
      }
      return;
    }
    client.markReconnecting();
    try {
      await client.resumeAfterReconnect();
    } catch (err) {
      this.reconcileToast(err, "会话同步");
    }
    // No global connection write here: only the connection machine publishes
    // link state. This manual path exists solely for the machine-less unit
    // harness/degraded mode; journalStatus already reflects the outcome.
    try {
      await this.refresh();
    } catch (err) {
      this.reconcileToast(err, "会话列表刷新");
    }
  }

  async create(spec: InstanceCreateSpec) {
    // D-055: create is not idempotent (no client key in the outbox, and the
    // caller navigates to /:id on success), so it is never queued offline.
    // Refuse when the link is down/recovering rather than optimistically
    // creating a row the UI cannot mount.
    if (this.connectionState === "offline" || this.connectionState === "recovering") {
      this.toast("当前无法连接 Hub，会话不会在离线时排队，恢复连接后再新建。");
      throw new Error("create refused while disconnected (D-055)");
    }
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
    // Pin the created row against every list request still in flight (a poll
    // started before the create resolves late and otherwise drops the row,
    // which made /s/:id render 会话不存在 immediately after navigation). The
    // pin releases on the first post-create response once all older requests
    // settle — see refresh()/mergeInstanceSnapshots.
    this.pinnedCreates.set(createdId, { seq: this.listReqSeq, confirmedByNewer: false });
    // Do not gate navigation on the list refresh: the create response already
    // carries the instance the new /s/:id route mounts, and the follow socket
    // delivers the rest. Under gate load awaiting the two refresh GETs here
    // kept NewSessionPage on /sessions/new past the test's 20 s window even
    // though the create had landed (a retry then passed). A failure is
    // surfaced (not swallowed): the 2 s poll self-heals the list.
    void this.refresh().catch((err) => this.reconcileToast(err, "会话列表刷新"));
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
    // previews so the token renders with the attachment.
    const indexOf = new Map(attachments.map((ref) => [ref.objectId, ref.index]));
    const numberedPreviews = previews.map((preview) => {
      const index = indexOf.get(preview.objectId);
      return index ? { ...preview, index } : preview;
    });
    const clientRequestId = id("local_");

    // D-055: generate the wire commandId BEFORE the POST and persist the row
    // first. Offline steer cannot keep its turn-specific meaning after a
    // disconnect, so it degrades to an ordinary queued turn.
    const offline = this.connectionState !== "live";
    const effectiveMode: PromptMode | undefined = offline && mode === "steer" ? undefined : mode;
    const persistedMode =
      effectiveMode && effectiveMode !== "new-turn" ? effectiveMode : undefined;

    const haveOutbox = await this.ensureOutbox();
    const commandId = haveOutbox ? newCommandId() : null;
    const createdAtMs = Date.now();
    const journalIdForRow = this.state.instances.find((i) => i.id === instanceId)?.journalId;
    // Persist the upload manifest refs (objectId/index/kind/…), never the
    // blob-only preview urls: the outbox survives reloads and POSTs the
    // canonical refs. The rendered bubble keeps the local preview urls.
    const persistableAttachments = attachments.filter(
      (a): a is AttachmentRef => Boolean(a.objectId),
    );
    if (this.outbox && commandId) {
      // Storage-first: the bubble renders only AFTER the durable commit. A
      // transaction abort left nothing in the cache/store, so retry the SAME
      // commandId once; a second abort is an honest failure, never a phantom
      // bubble or a second id for the intent.
      const enqueueRecord = {
        commandId,
        clientRequestId,
        instanceId,
        ...(journalIdForRow ? { journalId: journalIdForRow } : {}),
        ...(this.state.instances.find((i) => i.id === instanceId)
          ? { instanceSnapshot: this.state.instances.find((i) => i.id === instanceId) }
          : {}),
        prompt,
        ...(persistableAttachments.length ? { attachments: persistableAttachments } : {}),
        ...(persistedMode ? { mode: persistedMode } : {}),
        createdAt: createdAtMs,
      };
      try {
        await this.outbox.enqueue(enqueueRecord);
      } catch {
        try {
          await this.outbox.enqueue(enqueueRecord);
        } catch {
          return false;
        }
      }
    }
    const bubble: LocalBubble = {
      clientRequestId,
      instanceId,
      text: prompt,
      ...(numberedPreviews.length ? { attachments: numberedPreviews } : {}),
      // The client-generated id is the commandId from the first POST on;
      // Hub/Node dedup is what makes same-id retries exactly-once.
      commandId,
      state: "queued",
      outboxState: this.outbox ? "pending" : undefined,
      ...(effectiveMode ? { promptMode: effectiveMode } : {}),
      createdAt: new Date(createdAtMs).toISOString(),
    };
    this.emit({ bubbles: this.state.bubbles.concat(bubble) });

    if (!this.outbox || !commandId) {
      // No durable store (locked-down browser): legacy direct POST, and its
      // failure is honestly 状态待确认 — never an optimistic retry.
      return this.sendWithoutOutbox(instanceId, prompt, attachments, effectiveMode, clientRequestId);
    }
    if (this.connectionState !== "live") {
      // The row stays pending; the connection machine flushes it on recovery.
      // An offline steer degraded to a queued turn must NOT report success:
      // it has not interrupted anything, so no 已打断 receipt (and annotation
      // drafts stay until it really lands).
      return !(mode === "steer");
    }
    // Online: flush now in the background; the POST never blocks the caller.
    void this.flushAllOutbox();
    return true;
  }

  /** Legacy direct-POST path used only when durable storage is unavailable. */
  private async sendWithoutOutbox(
    instanceId: Id,
    prompt: string,
    attachments: AttachmentRef[],
    mode: PromptMode | undefined,
    clientRequestId: Id,
  ): Promise<boolean> {
    try {
      const result = await api.instanceSend(instanceId, prompt, attachments, mode);
      this.emit({
        bubbles: this.state.bubbles.map((b) =>
          b.clientRequestId === clientRequestId
            ? { ...b, state: result.command.state, commandId: result.command.commandId }
            : b,
        ),
      });
      const events = this.state.events[instanceId] ?? [];
      this.settleFromJournal(instanceId, events);
      void this.catchup(instanceId);
      void this.refreshScreen(instanceId).catch(() => undefined);
      return true;
    } catch {
      this.emit({
        bubbles: this.state.bubbles.map((b) =>
          b.clientRequestId === clientRequestId ? { ...b, state: "unknown" } : b,
        ),
      });
      return false;
    }
  }

  async retract(bubbleId: Id) {
    const bubble = this.state.bubbles.find((b) => b.clientRequestId === bubbleId);
    if (!bubble || bubble.state === "accepted" || bubble.state === "settled") return;
    // Remove the durable outbox row too if it never left (a queued offline
    // message cancelled before delivery); an inflight/done row cannot be
    // unsent, so leave it to its journal outcome.
    if (bubble.commandId && this.outbox) {
      const rec = this.outbox.get(bubble.commandId);
      if (rec && (rec.state === "pending" || rec.state === "rejected" || rec.state === "unknown")) {
        await this.outbox.remove(bubble.commandId);
      }
    }
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
    // Held rows are user-staged prompts; D-055 routes them through the same
    // durable outbox as normal sends (client commandId, same-id retry). The
    // COMPLETION of each enqueue is awaited before the network flush starts so
    // a steer of a later row clicked in the same burst cannot overtake an
    // earlier row's persistence; the flush chain itself serialises the POSTs
    // in createdAt order.
    if (!(await this.ensureOutbox())) return;
    const items = this.heldBubbles(instanceId);
    for (const item of items) {
      const current = this.state.bubbles.find((b) => b.clientRequestId === item.clientRequestId);
      if (!current || !current.held || current.state !== "queued") continue;
      // A persist abort leaves the row held with its lifetime commandId
      // already bound; skip it this flush (the next trigger retries the SAME
      // id) rather than aborting delivery of the remaining held rows.
      try {
        await this.enqueueBubble(current, { steer: false });
      } catch {
        /* retried under the same commandId on the next flush */
      }
    }
    await this.flushAllOutbox();
  }

  /**
   * Persist an (optionally held) optimistic bubble into the outbox and drop
   * its hold marker. A steer keeps its mode while live; offline it degrades to
   * a queued turn (the running turn it targeted may be over by recovery).
   *
   * Idempotent per clientRequestId: concurrent callers (a turn-end flush and
   * a steer of the same row) share one in-flight enqueue and therefore one
   * commandId. If the bubble was already converted (it carries a commandId /
   * has an outbox row), only the mode is upgraded — never a new id.
   */
  private enqueueBubble(bubble: LocalBubble, opts: { steer: boolean }): Promise<Id> {
    const inFlight = this.enqueueInFlight.get(bubble.clientRequestId);
    if (inFlight) return inFlight;
    const job = this.doEnqueueBubble(bubble, opts);
    this.enqueueInFlight.set(bubble.clientRequestId, job);
    // Swallow the cleanup chain's own rejection: callers receive `job` itself
    // and own its error; this tail must not surface as an unhandled rejection.
    void job
      .catch(() => undefined)
      .finally(() => this.enqueueInFlight.delete(bubble.clientRequestId));
    return job;
  }

  private async doEnqueueBubble(bubble: LocalBubble, opts: { steer: boolean }): Promise<Id> {
    // Re-read the live bubble: a racing earlier conversion may already have
    // assigned its commandId and dropped the hold.
    const live =
      this.state.bubbles.find((b) => b.clientRequestId === bubble.clientRequestId) ?? bubble;
    // Already converted once: reuse the existing id. If this call is a steer,
    // promote the queued row's mode (it cannot jump if it is already inflight).
    if (live.commandId && !live.held) {
      if (opts.steer && this.outbox) {
        const rec = this.outbox.get(live.commandId);
        if (rec && rec.state === "pending" && rec.mode !== "steer") {
          const steerMode = this.connectionState === "live" ? "steer" : undefined;
          await this.outbox.patch(live.commandId, steerMode ? { mode: steerMode } : { mode: "queue" });
          if (!steerMode) this.toast("离线时插话将改为排队发送，恢复后按普通消息送出");
          this.emit({
            bubbles: this.state.bubbles.map((b) =>
              b.clientRequestId === bubble.clientRequestId
                ? { ...b, promptMode: (steerMode ?? "new-turn") as PromptMode }
                : b,
            ),
          });
        }
      }
      return live.commandId;
    }

    // First (and only) commandId for this bubble's lifetime. Bind it to the
    // bubble BEFORE the first persistence attempt: if that attempt aborts the
    // row never existed durably, but the retry (a later flushHeld / steerHeld
    // of the still-held bubble) must carry this SAME id — one user intent,
    // one commandId, even across transaction aborts.
    const commandId = live.commandId ?? newCommandId();
    if (!live.commandId) {
      this.emit({
        bubbles: this.state.bubbles.map((b) =>
          b.clientRequestId === live.clientRequestId ? { ...b, commandId } : b,
        ),
      });
    }
    const createdAtMs = Date.now();
    const offline = this.connectionState !== "live";
    // opts.steer decides the mode (the held row itself carries promptMode
    // "queue" from hold() and must not win); offline steer degrades.
    const effectiveMode: PromptMode | undefined = opts.steer
      ? offline
        ? undefined
        : "steer"
      : live.promptMode;
    const mode: "queue" | "steer" | undefined = opts.steer ? (offline ? "queue" : "steer") : undefined;
    if (offline && opts.steer) {
      this.toast("离线时插话将改为排队发送，恢复后按普通消息送出");
    }
    const instanceForRow = this.state.instances.find((i) => i.id === live.instanceId);
    const journalIdForRow2 = instanceForRow?.journalId;
    try {
      await this.outbox?.enqueue({
        commandId,
        clientRequestId: live.clientRequestId,
        instanceId: live.instanceId,
        ...(journalIdForRow2 ? { journalId: journalIdForRow2 } : {}),
        ...(instanceForRow ? { instanceSnapshot: instanceForRow } : {}),
        prompt: live.text,
        ...(live.heldRefs?.length ? { attachments: live.heldRefs } : {}),
        ...(mode ? { mode } : {}),
        createdAt: createdAtMs,
      });
    } catch {
      // Persistence failed (IndexedDB transaction aborted): the row was never
      // durable and the outbox cache has no entry, but the bubble stays held
      // (or queued) WITH its bound commandId; the next flush/steer retries the
      // SAME intent under that id rather than silently minting a second one.
      throw new Error("failed to persist queued message");
    }
    // Synchronously claim the bubble (held dropped, commandId bound) so a
    // racing second caller after this await sees the conversion.
    this.emit({
      bubbles: this.state.bubbles.map((b) =>
        b.clientRequestId === live.clientRequestId
          ? {
              ...b,
              held: false,
              commandId,
              outboxState: "pending" as const,
              promptMode: effectiveMode ?? "new-turn",
            }
          : b,
      ),
    });
    return commandId;
  }

  /**
   * c-steer 插队发送: take ONE held row and send it NOW as a steer. The row is
   * persisted with its client commandId first (D-055); while live the POST is
   * awaited so the 已打断 receipt still means "the interrupt landed", and a
   * failure is honest. Offline, it degrades to a queued turn that recovery
   * flushes. A second call for the same id finds no held row.
   */
  async steerHeld(instanceId: Id, bubbleId: Id): Promise<boolean> {
    const bubble = this.state.bubbles.find(
      (b) => b.clientRequestId === bubbleId && b.instanceId === instanceId && b.state === "queued",
    );
    if (!bubble) return false;
    if (!(await this.ensureOutbox()) || !this.outbox) return false;

    // enqueueBubble is idempotent: it assigns the single commandId if still
    // held, or promotes the existing queued row to steer; a racing
    // flushHeld shares the same id.
    let commandId: Id | null = null;
    try {
      commandId = await this.enqueueBubble(bubble, { steer: true });
    } catch {
      return false;
    }
    if (!commandId) return false;

    // Offline: the row is queued (and degraded from steer to an ordinary
    // turn), but nothing was interrupted — report false so the Composer does
    // not raise the 已打断 receipt; recovery flushes the queued row.
    if (this.connectionState !== "live") return false;
    // Deliver the (steer-promoted) row through the single-deliverer lock.
    try {
      await this.outbox.withInstanceLock(instanceId, (id, rows) => this.deliverOneRow(id, rows));
    } catch {
      // Lock/lease failure: never claim the interrupt (the bounded retry /
      // reconnect delivers the same id later).
      return false;
    }
    this.syncBubbleFromOutbox(commandId);
    // Receipt only on AUTHORITATIVE acceptance (sent/done), never while merely
    // queued/reconciling/offline.
    const finalState = this.outbox.get(commandId)?.state;
    return finalState === "done" || finalState === "sent";
  }

  async close(instanceId: Id) {
    await api.instanceClose(instanceId);
    await this.refresh();
  }

  /**
   * D-028 §5.3: interrupt the current turn; session and process stay alive.
   * D-055: cancel is time-sensitive and non-idempotent across turns, so it is
   * never queued offline — the UI gates the button on {@link canInterrupt}.
   */
  async cancel(instanceId: Id): Promise<void> {
    if (!this.canInterrupt) {
      this.toast("离线时不可打断：恢复连接后再操作");
      return;
    }
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

  /**
   * Build the `screens` state patch for one screen update under the single
   * ordering described on {@link ScreenEntry}, or null when the candidate is
   * stale and the committed screen must stand. `lastLines(…, 3)` previews are
   * the caller's responsibility (the journal path keeps its wider tail).
   */
  private screenCommitPatch(
    instanceId: Id,
    lines: string[],
    done: boolean,
    source: { kind: "journal"; seq: string } | { kind: "rpc"; basis: string | null },
  ): Record<string, ScreenEntry> | null {
    const current = this.state.screens[instanceId] ?? null;
    if (source.kind === "journal") {
      // Catch-up re-derives the latest journal screen on every batch; an RPC
      // buffer already fresh through this seq (equal basis) is newer, and a
      // frame with a higher committed basis is newer still.
      if (current?.journalSeq != null && BigInt(current.journalSeq) >= BigInt(source.seq)) return null;
    } else if (current?.journalSeq != null) {
      // RPC read: ANY committed journal-derived screen outranks it when the
      // read's basis is null (no screen had been observed when the read
      // started — a newer journal frame must win), or when the journal seq is
      // strictly past a non-null basis. A null-basis RPC may never overwrite a
      // journal screen or reset its seq (an old DONE buffer replacing current
      // working content).
      if (source.basis == null || BigInt(current.journalSeq) > BigInt(source.basis)) {
        return null;
      }
    }
    const journalSeq = source.kind === "journal" ? source.seq : source.basis;
    return { ...this.state.screens, [instanceId]: { lines, done, journalSeq } };
  }

  /**
   * Per-instance serial chain for catch-up → screen reconciliation. Send
   * resolves immediately, but the background work must be ordered so a screen
   * RPC never overtakes a journal resync that carries unseen screen history
   * (an older journal screen could otherwise overwrite a newer live buffer).
   */
  private reconcileChain = new Map<Id, Promise<unknown>>();
  /** Per-instance timer for retrying sends that failed transiently while live. */
  private outboxRetryTimer = new Map<Id, ReturnType<typeof setTimeout>>();
  private outboxRetryAttempt = new Map<Id, number>();
  /** Bounded retry timer for Hub-held (host offline) rows; no attempt cost. */
  private heldRetryTimer = new Map<Id, ReturnType<typeof setTimeout>>();
  private heldRetryAttempt = new Map<Id, number>();
  /**
   * In-flight/outbox-keyed conversion of a held bubble, keyed by
   * clientRequestId. A held prompt gets exactly ONE commandId for its
   * lifetime: a turn-end `flushHeld` racing a `steerHeld` of the same row
   * awaits the SAME enqueue instead of minting a second id (HIGH: two ids for
   * one user action can't be deduped).
   */
  private enqueueInFlight = new Map<Id, Promise<Id>>();

  private scheduleOutboxRetry(instanceId: Id) {
    if (this.outboxRetryTimer.has(instanceId)) return;
    const n = this.outboxRetryAttempt.get(instanceId) ?? 0;
    // Same full-jitter shape as the reconnect backoff, capped at 30 s.
    const delay = Math.floor(Math.random() * Math.min(30_000, 500 * 2 ** n));
    this.outboxRetryAttempt.set(instanceId, n + 1);
    const timer = setTimeout(() => {
      this.outboxRetryTimer.delete(instanceId);
      // The timer is armed only by a real enqueue (authenticated UI); a stale
      // device session answers 401 and the response path handles logout.
      void this.flushAllOutbox();
    }, delay);
    this.outboxRetryTimer.set(instanceId, timer);
  }

  /**
   * Bounded retry for a Hub-held row (Node offline). Re-POSTs the SAME id (Task
   * B forwards an un-forwarded row once the host is back) without spending the
   * 20-attempt send budget; capped in time by OUTBOX_MAX_AGE_MS.
   */
  private scheduleHeldRetry(instanceId: Id) {
    if (this.heldRetryTimer.has(instanceId)) return;
    const n = this.heldRetryAttempt.get(instanceId) ?? 0;
    const delay = Math.min(HELD_RETRY_MAX_MS, HELD_RETRY_BASE_MS * 2 ** n);
    this.heldRetryAttempt.set(instanceId, n + 1);
    const timer = setTimeout(() => {
      this.heldRetryTimer.delete(instanceId);
      void this.flushAllOutbox();
    }, delay);
    this.heldRetryTimer.set(instanceId, timer);
  }

  private clearOutboxRetry(instanceId: Id) {
    const t = this.outboxRetryTimer.get(instanceId);
    if (t) clearTimeout(t);
    this.outboxRetryTimer.delete(instanceId);
    this.outboxRetryAttempt.delete(instanceId);
  }

  private chainReconcile(instanceId: Id, job: () => Promise<unknown>): Promise<unknown> {
    const prev = this.reconcileChain.get(instanceId) ?? Promise.resolve();
    const next = prev.then(job, job);
    this.reconcileChain.set(instanceId, next);
    void next.finally(() => {
      if (this.reconcileChain.get(instanceId) === next) this.reconcileChain.delete(instanceId);
    });
    return next;
  }

  async refreshScreen(instanceId: Id) {
    // Ordering vs catch-up is enforced by the generation guard plus the basis
    // (computed from BOTH committed screens and observed live events): a
    // journal frame arriving during the read makes the RPC stale even from a
    // list poll (item 10). Not chained — chaining a read that the catch-up
    // itself triggers would deadlock the per-instance chain.
    // Generation guard: a newer read (or the periodic scheduler) superseding
    // this one makes its late resolution a no-op — including its error.
    const gen = (this.screenReadGen.get(instanceId) ?? 0) + 1;
    this.screenReadGen.set(instanceId, gen);
    // Basis = the greatest journal seq the browser KNOWS before the RPC,
    // whether it was committed as a screen entry, carried as a screen
    // observation, or seen as ANY other event (the carried-review gap: a seed
    // screen never written to `screens` left the basis null; the list-poll gap:
    // events known through N with no screen observation left it null too). The
    // live buffer is fresh through at least this seq, so a delayed journal
    // screen at/below it — including one delivered by a late gap fill — cannot
    // roll the buffer back. A journal frame strictly above the basis still wins.
    const basisSeq = (() => {
      const committed = this.state.screens[instanceId]?.journalSeq ?? null;
      const observed = latestScreenSnapshot(this.state.events[instanceId] ?? []).seq;
      let known: string | null = null;
      for (const ev of this.state.events[instanceId] ?? []) {
        if (known === null || BigInt(ev.seq) > BigInt(known)) known = ev.seq;
      }
      let basis: string | null = null;
      for (const candidate of [committed, observed, known]) {
        if (candidate != null && (basis === null || BigInt(candidate) > BigInt(basis))) basis = candidate;
      }
      return basis;
    })();
    let read: { lines: string[] };
    try {
      read = await api.screenRead(instanceId, 80);
    } catch (err) {
      if (this.screenReadGen.get(instanceId) !== gen) return;
      // NODE_BUSY is bulk-read back-pressure (the scheduler backs off);
      // anything else is an unexpected failure that api.screenRead no longer
      // converts into an empty screen — propagate so the caller reports it
      // instead of silently keeping or re-deriving content.
      throw err;
    }
    if (this.screenReadGen.get(instanceId) !== gen) return;
    let patch: Record<string, ScreenEntry> | null;
    if (!read.lines.length) {
      // Legitimately no live buffer (unsupported carrier / offline host
      // answers 4xx at the API layer): fall back to the latest journal screen.
      const snapshot = latestScreenSnapshot(this.state.events[instanceId] ?? []);
      if (!snapshot.lines.length || snapshot.seq === null) return;
      patch = this.screenCommitPatch(
        instanceId,
        lastLines(snapshot.lines, 3),
        doneFromLines(snapshot.lines),
        { kind: "journal", seq: snapshot.seq },
      );
    } else {
      // Any journal frame with a seq beyond the RPC's basis that committed
      // during the flight makes the read stale (screenCommitPatch enforces
      // it); the next scheduled poll re-reads the current buffer.
      const lines = lastLines(read.lines, 80);
      patch = this.screenCommitPatch(instanceId, lastLines(lines, 3), doneFromLines(read.lines), {
        kind: "rpc",
        basis: basisSeq,
      });
    }
    if (patch) this.emit({ screens: patch });
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
      if (!isScreenNodeBusy(err)) {
        // An unexpected screen-read failure (5xx/network) must not be
        // swallowed: the periodic list poll retries the row on its own, and
        // this advisory surfaces the gap instead of presenting stale content
        // as current.
        this.reconcileToast(err, "屏幕同步");
        return;
      }
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
      // The decision is committed the moment the POST succeeds. Any failure
      // AFTER that (a list/catchup refresh) is a UI-sync problem, not a
      // rejected decision: keep the success path so the card does not flip
      // back to answerable (a second answer would only be Superseded).
      await api.interactionRespond(interactionId, answer);
    } catch (error) {
      // Only a rejected POST returns the card to answerable state and surfaces
      // the error; the draft is preserved for correction.
      const { [interactionId]: _removed, ...rest } = this.state.answering;
      this.emit({ answering: rest });
      throw error;
    }
    try {
      await this.refresh();
      const interaction = this.state.interactions.find((i) => i.id === interactionId);
      if (interaction) await this.catchup(interaction.instanceId);
    } catch {
      // Post-commit sync failure: the broker already accepted the answer.
      // Leave the card in its submitted/committed state; do not re-enable the
      // buttons or make the reviewer think the decision was rejected.
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
    // The *picker* selection: optimistic launch/configure, a terminal
    // `/model` folded in by `noteModelObservation`, the launch spec, or the
    // picker's built-in alias. The session model chip does not read this —
    // it shows the recorded running/launch value verbatim (`runningModelOf`).
    return (
      this.state.models[instanceId] ??
      instance?.model ??
      (kind === "codex" ? "gpt-5" : kind === "grok" ? "grok-4" : "opus")
    );
  }

  /** The model the chip shows, verbatim: the transcript-read-back RUNNING
   *  model; before any read-back the durable launch spec; null when neither
   *  exists. Nothing is invented (no opus/gpt-5/grok-4), no aliasing. */
  runningModelOf(instanceId: Id): string | null {
    const effective = this.state.modelEffective[instanceId]?.id;
    if (effective) return effective;
    const instance = this.state.instances.find((row) => row.id === instanceId);
    return instance?.model ? instance.model : null;
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
    // Settle ONLY by commandId (D-055). Text matching was removed: a held,
    // not-yet-sent row whose text repeats a prior message (e.g. another
    // "continue") used to be marked settled by that unrelated history event
    // and silently dropped before it was ever POSTed. A row without a
    // commandId (unconverted hold) is never settled here.
    if (!bubble.commandId) return bubble;
    const matchedEvent = events.find(
      (ev) =>
        ev.kind === "message" &&
        (ev.payload as { commandId?: Id }).commandId === bubble.commandId,
    );
    if (matchedEvent) {
      // The optimistic bubble is about to be filtered out of the rendered
      // list. Carry its local-only attachment thumbnails onto the matching
      // journal event so the assembled (authoritative) node keeps them —
      // the journal never echoes blob preview urls back.
      if (bubble.attachments?.length) {
        (matchedEvent.payload as { localAttachments?: BubbleAttachment[] }).localAttachments ??=
          bubble.attachments;
      }
      return { ...bubble, state: "settled" as const, outboxState: "done" };
    }
    return bubble;
  });
}

export const hubStore = new HubStore();

export function useHub(): HubState {
  return useSyncExternalStore(hubStore.subscribe, hubStore.getSnapshot, hubStore.getSnapshot);
}
