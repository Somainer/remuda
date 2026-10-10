import { useSyncExternalStore } from "react";
import { profileRegion } from "./profileFlags";
import { structuralEqual } from "./structuralEqual";

/** Bounded journal tail the list reads per live instance to project its phrase. */
const SUMMARY_TAIL = 64;
import type { Command, CommandSettlementOutcome } from "../types/command";
import type { components } from "./api.generated";
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
import { e2eSeamsEnabled } from "./e2eSeams";
import { JournalClient, type JournalRead } from "./journal";
import { id, localId, now } from "./ids";
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
  /**
   * `observedAt` of the configure lifecycle that put the switch in the
   * queue. A queued switch must be cleared ONLY by an effective read-back at
   * least as new as the queued REQUEST: a projection newer than the old
   * baseline but older than the request (a stale in-flight poll) must not
   * settle it. Null for an optimistic setEffort that has not been journaled
   * yet (falls back to the baseline rule).
   */
  requestObservedAt: string | null;
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
  /**
   * The Node/Hub's own reason for a definite rejection (the outbox row's
   * lastError, e.g. settlement.reason), shown inline next to 未送达 as
   * neutral text in the row — never a toast. Present only for a rejected
   * row; other states carry no user-facing reason here.
   */
  outboxError?: string;
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
  /**
   * Whether the interaction list has been loaded from at least one
   * SUCCESSFUL interaction-list response (bootstrap or poll). A failed first
   * fetch leaves this false even though `ready` is emitted with an empty list
   * (offline reload), so arrival watchers (question alerts) do not take a
   * baseline from a failed fetch and toast every already-pending question on
   * the next successful poll. Empty is a valid success and sets this true.
   */
  interactionsHydrated: boolean;
  events: Record<string, Observation[]>;
  journalStatus: Record<string, JournalClient["status"]>;
  /**
   * Lowest LOADED (retained) journal seq per instance, mirrored from the
   * JournalClient's descending pager. The transcript's load-earlier button
   * points at THIS floor rather than the min event seq: a reconnect snapshot
   * re-anchors the server window above manually paged rows, which stay held.
   * "1" (or absent for an unfollowed instance) hides the button.
   */
  journalFloors: Record<string, string>;
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
  interactionsHydrated: false,
  events: {},
  journalStatus: {},
  journalFloors: {},
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
/**
 * True when the merge produced no element changes (same length, every row
 * still reference-identical at the same index). Pair with the identity
 * preservation inside the merge functions: an unchanged poll parses fresh
 * JSON objects but the merge maps each one back onto the prior row, so this
 * check lets the caller reuse the previous ARRAY reference and skip the
 * store emission entirely (c-perffu: a quiet 2 s poll must not re-render).
 */
function sameArrayIdentity<T>(next: readonly T[], prev: readonly T[]): boolean {
  if (next.length !== prev.length) return false;
  for (let i = 0; i < next.length; i += 1) {
    if (next[i] !== prev[i]) return false;
  }
  return true;
}

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
    if (newer && BigInt(newer.durableSeq) > BigInt(instance.durableSeq)) return newer;
    // Equal content from a fresh JSON parse: keep the prior object identity so
    // an unchanged poll is a no-op for every memoized consumer.
    if (newer && structuralEqual(newer, instance)) return newer;
    return instance;
  });
  if (!pins?.size) return merged;
  const seen = new Set(merged.map((instance) => instance.id));
  for (const [id, pin] of pins) {
    if (seen.has(id)) continue;
    const optimistic = previous.get(id);
    if (!optimistic) continue;
    // This response (in `outstanding`), or an even older request still
    // pending, may predate the create: keep the optimistic row.
    if (Array.from(outstanding ?? [reqSeq]).some((seq) => seq <= pin.seq)) {
      merged.unshift(optimistic);
    }
  }
  return merged;
}

/**
 * Merge an authoritative interaction list page with the current projection.
 *
 * Terminal state never regresses to `pending`: when an interaction was
 * locally settled from an answer receipt (`settled` pins the list-request seq
 * at POST success), a delayed older poll that still returns the pending row
 * must not re-enable its buttons, nor insert it when a newer page already
 * removed it. A response whose `reqSeq` is at or before the pin may predate
 * the answer:
 *  - a pending copy of the pinned id is dropped (or shadowed by the committed
 *    local row — never resurrected when the row was already removed);
 *  - an omitted id keeps its committed local row while this response or any
 *    older sibling is still in flight (same pin discipline as
 *    {@link mergeInstanceSnapshots}).
 * Only a strictly newer response releases the pin (the caller marks it), and
 * the row is then dropped/confirmed normally.
 */
function mergeInteractionSnapshots(
  incoming: Interaction[],
  current: Interaction[],
  settled: ReadonlyMap<Id, { seq: number; confirmedByNewer: boolean }>,
  reqSeq = Number.POSITIVE_INFINITY,
  outstanding?: ReadonlySet<number>,
): Interaction[] {
  const previous = new Map(current.map((row) => [row.id, row]));
  const terminalOrder: Record<string, number> = {
    pending: 0,
    reconciling: 1,
    "answer-committed": 2,
    expired: 3,
    invalidated: 3,
    resolved: 3,
    unknown: 0,
  };
  const rank = (state: Interaction["state"]) => terminalOrder[state] ?? 0;
  const merged: Interaction[] = [];
  for (const row of incoming) {
    const pin = settled.get(row.id);
    const prior = previous.get(row.id);
    // A page that started at or before the answer commit cannot carry an
    // authoritative pending copy: keep the committed projection, or swallow
    // the row entirely (tombstone) when no local row survives.
    if (pin && reqSeq <= pin.seq && row.state === "pending") {
      if (prior && rank(prior.state) > rank(row.state)) merged.push(prior);
      continue;
    }
    // Defense in depth for a newer page that still reports pending: terminal
    // never regresses while the settlement pin is held.
    if (
      prior &&
      settled.has(row.id) &&
      row.state === "pending" &&
      rank(prior.state) > rank(row.state)
    ) {
      merged.push(prior);
      continue;
    }
    // Unchanged row from a fresh JSON parse: preserve the prior identity.
    merged.push(prior && structuralEqual(prior, row) ? prior : row);
  }
  if (settled.size) {
    // Tombstone retention: the committed id is missing from this page. A
    // request at/before its pin (this one included) may simply predate the
    // commit, so retain the committed local row until a newer page confirms
    // settlement and every older request has resolved.
    const seen = new Set(merged.map((row) => row.id));
    for (const [id, pin] of settled) {
      if (seen.has(id)) continue;
      const committed = previous.get(id);
      if (!committed) continue;
      if (Array.from(outstanding ?? [reqSeq]).some((seq) => seq <= pin.seq)) {
        merged.push(committed);
      }
    }
  }
  return merged;
}

/**
 * The structural facts the outbox classifier needs, shared by the POST
 * envelope's rich {@link Command} (its `dispatch` states map to the ledger's
 * forward flag) and the bare GET CommandRecord (which carries `forwarded`).
 */
type CommandLikeForClassify = {
  state: "queued" | "accepted" | "settled" | string;
  resolution: "clear" | "unknown" | "reconciling" | string;
  settlement?: { outcome: CommandSettlementOutcome; reason?: string };
  forwarded: boolean;
};

/**
 * Classify a Hub command record into an outbox outcome.
 *  - rejected: settled with settlement.outcome rejected (real Node reject).
 *  - held: queued and never forwarded (Node offline); same-id re-POST later.
 *  - reconciling: FORWARDED but the Hub has no verdict yet — resolution
 *    "reconciling" OR "unknown". The Hub records the forward intent BEFORE
 *    awaiting the Node, so a lost POST can read the row back via GET as
 *    queued+forwarded+unknown; it is NOT proof of acceptance. Settled by a
 *    bounded GET (never re-forwarded).
 *  - sent: accepted/settled-completed or a clear forwarded row; reached the
 *    Hub/Node, await the journal join (never re-POST).
 */
function classifyCommandLike(
  command: CommandLikeForClassify,
): "sent" | "held" | "reconciling" | "rejected" {
  if (command.state === "settled" && command.settlement?.outcome === "rejected") {
    return "rejected";
  }
  if (command.state === "queued" && !command.forwarded) {
    return "held";
  }
  if (
    command.state === "queued" &&
    (command.resolution === "reconciling" || command.resolution === "unknown")
  ) {
    return "reconciling";
  }
  return "sent";
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
  /**
   * Interaction ids locally settled from a successful answer POST, keyed by
   * the list-request seq captured at POST success. A list response at or
   * before that seq may predate the commit and is never allowed to resurrect
   * the pending card (or re-insert it after a newer empty page removed the
   * row). The pin is released only once a strictly newer list page has
   * confirmed settlement (id omitted or reported non-pending) AND every older
   * outstanding request has resolved — the same seq/outstanding discipline as
   * `pinnedCreates`.
   */
  private settledInteractions = new Map<Id, { seq: number; confirmedByNewer: boolean }>();
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
  /**
   * Monotonic generation of {@link connectionBoundJournal}: bumped only when
   * the machine is (re)bound to a DIFFERENT journal. A follow mount captures
   * its generation and may settle the machine's attempt only while it is still
   * current — navigating A → B mid-seed supersedes A, whose late success must
   * not certify B and whose late failure must not take B offline.
   */
  private connectionBindGen = 0;
  /**
   * Instances whose last drained send skipped the post-delivery REST
   * catch-up because THIS instance's follow owned recovery at that moment.
   * The debt is keyed to the bind generation and resume attempt owning it:
   * the owning follow's successful resync honours it, while a failed reopen,
   * a failed resume attempt, a watchdog timeout of the named attempt, or a
   * bind-away navigation (the previous owner can never service it) schedules
   * one coalesced REST fallback for the instance so its accepted row can
   * still settle. A failed fallback KEEPS the debt for the next trigger.
   * Cleared only after a catch-up actually succeeds and folds the journal.
   */
  private reconcileOwed = new Map<Id, { bindGen: number; attemptId: number | null }>();
  /**
   * Instances with an owed REST fallback currently running. Several triggers
   * can fire for one instance while its bounded read is in flight (rebind,
   * failed reopen, watchdog — and N drained rows); they must collapse into the
   * ONE coalesced read the doc comment on runOwedReconcile promises instead of
   * queueing N reads behind each other (c-reconnfu gate 6 item 4a).
   */
  private owedReconcileRunning = new Set<Id>();
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
  /**
   * The row occupies a scheduler slot: pump-queued or actively reading. An id
   * is admitted at most once at any time, which bounds both the list-poll
   * fan-out and the post-delivery kicks to ONE active read per instance.
   */
  private screenPending = new Set<Id>();
  /** Subset of {@link screenPending} whose RPC is actually in flight. */
  private screenActive = new Set<Id>();
  /**
   * Exactly ONE coalesced follow-up demand per instance, recorded by a
   * post-delivery kick that landed while a read was active. Any number of
   * drained sends collapses to this single flag; it is re-armed once when the
   * active read settles. So while /screen is slow the per-instance bound is
   * one active read plus at most one pending refresh, however many sends land.
   */
  private screenCoalesced = new Set<Id>();
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
      const foldsEffective =
        !current ||
        view.observedAt > current.observedAt ||
        (view.observedAt === current.observedAt && !structuralEqual(current, view));
      // Pending settlement is evaluated INDEPENDENTLY of whether the effective
      // record itself changes. The live socket may already have folded an
      // equal effective while a historical queued-effort replay (onPrepend)
      // still carries pending: an identical poll must then clear pending even
      // though the effective map does not change.
      const pending = this.state.effortPending[instance.id];
      // Clear a queued switch only on a read-back at least as new as the
      // queued request (c-perffu r7): a projection newer than the pre-switch
      // baseline but older than the configure request is a stale poll and must
      // leave the pending switch in place. An optimistic (unjournaled)
      // pending with no request timestamp keeps the baseline-only rule.
      const settlesPending =
        pending != null &&
        (pending.requestObservedAt != null
          ? view.observedAt >= pending.requestObservedAt &&
            (!pending.baselineObservedAt || view.observedAt > pending.baselineObservedAt)
          : !pending.baselineObservedAt || view.observedAt > pending.baselineObservedAt);
      if (foldsEffective) {
        // Fold only on a strictly newer observation, or an equal-timestamp
        // record whose content actually changed — an identical fold is not an
        // emission (c-perffu: quiet polls render nothing).
        next[instance.id] = view;
        effectiveUpdated = true;
      }
      if (settlesPending) {
        delete pendingNext[instance.id];
        pendingSettled = true;
        // Remember the read-back that settled it so the same edge arriving
        // later on the live socket is not mistaken for a terminal switch.
        this.settledEffortPushdown.set(instance.id, view.observedAt);
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
      // Equal turn count with equal content is not a change; the server
      // rebuilds the rollup object on every read.
      if (
        !current ||
        rollup.turns > current.turns ||
        (rollup.turns === current.turns && !structuralEqual(current, rollup))
      ) {
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
        if (
          !current ||
          view.observedAt > current.observedAt ||
          (view.observedAt === current.observedAt && !structuralEqual(current, view))
        ) {
          effectiveNext[instance.id] = view;
          effUpdated = true;
        }
      }
      const catalog = catalogFromRecord(instance.modelCatalog);
      if (catalog) {
        // The catalog payload is rebuilt server-side per read; fold it only
        // when the content actually differs.
        if (
          catalogNext[instance.id] === undefined ||
          !structuralEqual(catalogNext[instance.id], catalog)
        ) {
          catalogNext[instance.id] = catalog;
          catUpdated = true;
        }
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
      if (
        !current ||
        view.observedAt > current.observedAt ||
        (view.observedAt === current.observedAt && !structuralEqual(current, view))
      ) {
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
        // Stamp the request with THIS configure event's time; keep the
        // earliest one if a queued switch re-announces. Settlement requires a
        // read-back no older than the request.
        requestObservedAt:
          pending[instanceId]?.requestObservedAt ?? observation.observedAt,
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
      // A forwarded-but-unresolved row ("reconciling", including a forward
      // whose response was lost and read back as queued+unknown) is
      // non-deliverable, so the flush cannot settle it: restart its bounded
      // GET reconciliation instead of leaving 状态待确认 on the row forever.
      // Never re-POSTs — the Hub already recorded the forward.
      for (const r of restored) {
        if (r.state === "reconciling") this.reconcileReconcilingRow(r.instanceId, r.commandId);
      }
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
      onAttemptFinish: (info) => {
        // gate 6 item 2: a hung resume the watchdog gave up on is a follow
        // that will never honour its drained-row debt. The bound instance may
        // still be reachable over REST, so fold it now rather than leaving the
        // accepted row at 已受理 through the reconnect retries. A rejection
        // ("failed") is handled where the resume action throws
        // (resumeConnection's catch); a superseded attempt never reports here.
        if (info.why !== "watchdog") return;
        const instanceId = this.connectionBoundTo;
        if (!instanceId) return;
        const debt = this.reconcileOwed.get(instanceId);
        if (debt && debt.bindGen === info.gen && (debt.attemptId === null || debt.attemptId === info.attemptId)) {
          this.runOwedReconcile(instanceId);
        }
      },
    });
    this.connection = machine;
    // A successful REST bootstrap proves reachability, but on the list/new
    // pages there is no follow socket to watchdog yet: start live WITHOUT a
    // frame timer. The instant a session's follow socket binds
    // (openFollowSocket → followBound) the framed-socket deadline takes over,
    // so an active session whose transcript is frozen can never show 已连接.
    // A failed bootstrap starts the offline reconnect loop instead.
    if (offline) machine.setStateOffline();
    else machine.bootstrapLive();

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
    this.unloadProceeded = false;
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

  /**
   * Probe for the CURRENT follow socket's readyState. The subscription object
   * keeps the live WebSocket; reading through it at every liveness check means
   * a socket that silently closed without firing our close handler is still
   * observed (sampling it once after the promise could leave a cached OPEN
   * forever — the owner's phone coming out of iOS background).
   */
  /**
   * Live socket probes and last-frame times, keyed by journalId. A session the
   * user navigated AWAY from keeps its socket open (its events still hydrate
   * the list), so liveness is scoped to the CURRENTLY BOUND journal only: an
   * inactive session's frames must never certify the active session's link,
   * and its close must never take the active session offline.
   */
  private followReadyState = new Map<Id, () => number>();
  private followFrameAt = new Map<Id, number>();

  /**
   * Bind the connection machine to a session. The generation bumps only when
   * the journal changes: the reconnect resume's nested follow() of the SAME
   * journal shares the in-flight attempt, while a user navigation A → B
   * supersedes any attempt A still owns. Returns the generation the mount
   * must report back with.
   */
  private bindConnection(instanceId: Id, journalId: Id): number {
    const reboundAway =
      this.connectionBoundJournal !== null && this.connectionBoundJournal !== journalId;
    const prevBoundTo = this.connectionBoundTo;
    if (reboundAway) this.connectionBindGen += 1;
    this.connectionBoundTo = instanceId;
    this.connectionBoundJournal = journalId;
    this.connection?.noteBinding(this.connectionBindGen);
    // The machine now serves a DIFFERENT session. If the instance we just left
    // had a delivered row whose catch-up was deferred to its (now superseded)
    // follow, that follow will never service it — run one coalesced REST
    // fallback for the abandoned instance so its accepted row still settles
    // (fix-5 item 1). The global offline/recovering flag alone is not
    // ownership; only a concrete bind-away orphans the debt.
    if (reboundAway && prevBoundTo && prevBoundTo !== instanceId && this.reconcileOwed.has(prevBoundTo)) {
      this.runOwedReconcile(prevBoundTo);
    }
    return this.connectionBindGen;
  }
  /**
   * The follow link is genuinely live only when the socket is OPEN AND a frame
   * arrived within LIVE_FRAME_MS. This is the ONLY thing that lets a
   * foreground resume trust a cached "live" instead of reopening.
   */
  private followSocketLive(): boolean {
    const journalId = this.connectionBoundJournal;
    if (!journalId) return false;
    const getReadyState = this.followReadyState.get(journalId);
    if (!getReadyState || getReadyState() !== 1) return false;
    const lastFrameAt = this.followFrameAt.get(journalId) ?? 0;
    if (!lastFrameAt) return false;
    return Date.now() - lastFrameAt <= LIVE_FRAME_MS;
  }

  /** Test-only: force the follow-live verdict (simulates open + fresh frame). */
  setFollowLiveForTest(open: boolean, framed: boolean) {
    const journalId = this.connectionBoundJournal ?? "test_journal";
    if (open) this.followReadyState.set(journalId, () => 1);
    else this.followReadyState.delete(journalId);
    if (framed) this.followFrameAt.set(journalId, Date.now());
    else this.followFrameAt.delete(journalId);
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

  /** Test-only: replace slices and emit, so stores subscribing to hubStore
   * (e.g. the question-alert watcher) can be driven without a live Hub. */
  setSlicesForTest(slices: Partial<HubState>): void {
    this.emit(slices);
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
  /**
   * Set by a genuine pagehide. The beforeunload self-reset task is queued at
   * setTimeout(0) and the browser's pagehide task can land AFTER it, so the
   * task cannot infer the prompt was cancelled from timer ordering alone: it
   * flushes only when pagehide never marked the navigation as real.
   */
  private unloadProceeded = false;
  /** Auto-clear for a beforeunload prompt the user CANCELS (page stays). */
  private unloadClearTimer: ReturnType<typeof setTimeout> | null = null;
  private onPageHide = (event?: Event) => {
    const proceeding = event?.type === "pagehide";
    this.pageIsUnloading = true;
    if (proceeding) {
      // Navigation really proceeded: stop every timer so a tearing-down page
      // cannot POST, and cancel the beforeunload self-reset. BFCache restore
      // re-arms from onConnPageshow; a hard reload starts a fresh document.
      this.unloadProceeded = true;
      for (const [, t] of this.outboxRetryTimer) clearTimeout(t);
      this.outboxRetryTimer.clear();
      for (const [, t] of this.leaseWakeupTimer) clearTimeout(t);
      this.leaseWakeupTimer.clear();
      this.leaseWakeupUntil.clear();
      if (this.unloadClearTimer) {
        clearTimeout(this.unloadClearTimer);
        this.unloadClearTimer = null;
      }
      return;
    }
    // beforeunload: disarm the retry/lease timers (they no-op anyway while
    // the flag is up through the imminent pagehide of a real reload, and
    // they must not POST during teardown). A CANCELLED prompt fires no
    // pagehide and the document stays alive: the self-reset below re-latches
    // delivery and re-arms the lease wakeups.
    for (const [, t] of this.outboxRetryTimer) clearTimeout(t);
    this.outboxRetryTimer.clear();
    for (const [, t] of this.leaseWakeupTimer) clearTimeout(t);
    this.leaseWakeupTimer.clear();
    this.leaseWakeupUntil.clear();
    // The reset runs at setTimeout(0): in Chromium it measurably fires ~0.1
    // ms BEFORE pagehide during a real reload, so pagehide marks
    // unloadProceeded but cannot cancel this task in time. The task
    // therefore must have NO side effect unsafe in a tearing-down page: it
    // only clears the flag (a later user send triggers its own flush) and
    // RE-ARMS WAKEUPS READ-ONLY — those timers just setTimeout at lease
    // expiry (seconds out); a real pagehide task clears them, and no POST
    // runs from here.
    this.unloadProceeded = false;
    if (this.unloadClearTimer) clearTimeout(this.unloadClearTimer);
    this.unloadClearTimer = setTimeout(() => {
      this.unloadClearTimer = null;
      if (this.unloadProceeded) return;
      this.pageIsUnloading = false;
      void this.rearmLeaseWakeups();
    }, 0);
  };

  /**
   * Re-arm lease-expiry wakeups from durable rows WITHOUT flushing: read-only
   * refresh + schedule. Used by the cancelled-beforeunload self-reset, which
   * must not itself POST (the task can run inside a real reload teardown).
   */
  private async rearmLeaseWakeups() {
    const box = this.outbox;
    if (!box || this.pageIsUnloading) return;
    const now = Date.now();
    const durable = await box.refreshDurable().catch(() => null);
    if (!durable || this.pageIsUnloading) return;
    // The prompt may have sat open PAST the foreign lease deadline: include
    // overdue leases so an immediate guarded wakeup delivers the stealable
    // row (the normal flush path skips them — it delivers them itself).
    this.scheduleLeaseExpiryWakeup(durable, now, box.ownerId, true);
  }

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
   * leaves the transcript frozen under a false live).
   *
   * Neither outbox delivery nor the post-delivery screen refresh gates the
   * reopen: deliverOneRow's journal resync rides the fast journal chain the
   * reopen joins, and the Node-bound /screen read rides a separate screen
   * chain (it can stall for its whole REST timeout + NODE_BUSY backoff under
   * load but must never hold the link). Awaiting the flush used to park the
   * reopen behind that read: the queued row had committed and journaled while
   * the link sat in recovering past its watchdog, so the journal banner never
   * cleared (c-reconnfu gate flake). The machine also re-flushes on live.
   */
  private async resumeConnection() {
    // Kick outbox delivery CONCURRENTLY, never as a gate. The flush is more
    // than POSTs: after the last row of an instance lands, deliverOneRow chains
    // a journal resync plus a SCREEN read, and reopenFollow opens the socket on
    // the journal chain. Screen reads are Node RPCs that can stall for their
    // whole REST timeout (plus NODE_BUSY backoff) under gate load, so they ride
    // a separate screen chain that waits BEHIND journal work but never blocks
    // it; the flush itself is likewise never awaited here. Awaiting either used
    // to park the reopen behind a parked /screen: the queued row had committed
    // and journaled while the link sat in recovering past its watchdog, so the
    // journal banner never cleared (c-reconnfu gate flake). Reopening first lets
    // the subscribe snapshot certify the socket as soon as the Hub is
    // reachable, and the machine re-flushes on the live transition regardless.
    const flushed = this.flushAllOutbox();
    void flushed.catch(() => undefined);
    try {
      if (this.connectionBoundTo) {
        // Throws on socket-open or catch-up failure → machine remains offline
        // and retries; no toast-swallowing into false live. The journal chain
        // never carries the Node-bound screen read, so this cannot stall behind
        // a saturated Node.
        await this.reopenFollow(this.connectionBoundTo);
      }
    } catch (err) {
      // gate 6 item 2: the bound follow just failed while it owned a drained
      // row's catch-up debt (proxy refusing the upgrade, subscribe timeout,
      // resync rejection). REST still works — honour the debt NOW instead of
      // leaving the row at 已受理 through every failed reopen. The machine
      // still receives the rejection and stays offline/retrying.
      if (this.connectionBoundTo && this.reconcileOwed.has(this.connectionBoundTo)) {
        this.runOwedReconcile(this.connectionBoundTo);
      }
      throw err;
    }
    // c-reconnfu gate 7: the trailing REST bootstrap must NOT decide the
    // resume outcome. Under gate load the follow socket can be open and
    // streaming while the /v1/instances list read is slow/fails; rejecting
    // (or succeeding) here only certifies/denies the LINK via the machine —
    // a failed list read is not a dead follow (the socket's frame watchdog
    // and close own that), and a successful read does not certify live.
    // Treating it as either flapped the machine offline↔recovering on every
    // resume while the restored page's follow was otherwise healthy.
    void Promise.all([this.refresh().catch(() => undefined), this.refreshHosts().catch(() => undefined)]);
  }

  /**
   * Whether the bound follow for `instanceId` currently OWNS its post-delivery
   * journal catch-up: the connection machine is bound to that instance and is
   * mid-recovery, so reopenFollow's resync is authoritative. Evaluated AT JOB
   * EXECUTION (not when the drain enqueued), so a state that flips
   * live→recovering while the job waited behind an earlier slow catch-up is
   * read correctly instead of using a stale decision.
   */
  private followOwnsCatchupNow(instanceId: Id, journalId: Id | null): boolean {
    return (
      journalId !== null &&
      this.connectionBoundJournal === journalId &&
      this.connectionBoundTo === instanceId &&
      (this.connectionState === "offline" || this.connectionState === "recovering")
    );
  }

  /**
   * Record post-delivery catch-up debt against the resume attempt CURRENTLY
   * owning the bound instance (c-reconnfu gate 6 item 2). A debt stamped with
   * the live attempt is honoured when THAT attempt fails or its watchdog
   * fires; a null attempt id (recorded while offline between attempts) is
   * honoured by any attempt-end for the binding.
   */
  private recordOwed(instanceId: Id) {
    const ref = this.connection?.attemptRef() ?? null;
    this.reconcileOwed.set(instanceId, {
      bindGen: this.connectionBindGen,
      attemptId: ref?.attemptId ?? null,
    });
  }

  /**
   * Honour a post-delivery catch-up for an instance that NO follow currently
   * owns (the binding moved to another session) or whose owning attempt just
   * failed/timed out. One bounded REST resume per instance on its own journal
   * chain; never drives the global connection machine (it is bound elsewhere
   * or the follow was just judged dead). Idempotent — safe to call when a
   * follow later takes over.
   *
   * The debt is deleted ONLY after a catch-up that actually reaches the
   * journal whole (resumeAfterReconnect resolves): a read rejection /
   * readonly-stale settlement keeps it so the next failure, watchdog, rebind
   * or navigation trigger retries instead of leaving the accepted row stuck
   * at 已受理 (c-reconnfu gate 6 item 2).
   */
  private runOwedReconcile(instanceId: Id) {
    if (!this.reconcileOwed.has(instanceId)) return;
    // Coalesce: N drained rows / several failure triggers for one instance
    // must collapse into ONE bounded read, not N jobs queued on the chain
    // (gate 6 item 4a). The running read clears the debt on success; on
    // failure the debt stays and the next trigger starts another read.
    if (this.owedReconcileRunning.has(instanceId)) return;
    this.owedReconcileRunning.add(instanceId);
    void this.chainReconcile(instanceId, async () => {
      try {
        if (!this.reconcileOwed.has(instanceId)) return;
        const instance =
          this.state.instances.find((i) => i.id === instanceId) ??
          (await api.instanceGet(instanceId).catch(() => null));
        const client = instance ? this.journals.get(instance.journalId) : undefined;
        if (!client) {
          // Nothing mounted to fold; keep the debt for the mount/trigger that
          // can actually catch this instance up.
          return;
        }
        try {
          await client.resumeAfterReconnect();
        } catch {
          /* bounded REST catch-up failed: keep the debt for the next trigger */
          return;
        }
        this.reconcileOwed.delete(instanceId);
        this.settleFromJournal(instanceId, this.state.events[instanceId] ?? []);
      } finally {
        this.owedReconcileRunning.delete(instanceId);
      }
    });
  }

  /** Reopen the follow socket for an already-mounted instance and resync. */
  private async reopenFollow(instanceId: Id) {
    const instance =
      this.state.instances.find((i) => i.id === instanceId) ?? (await api.instanceGet(instanceId));
    const client = this.journals.get(instance.journalId);
    // Join the JOURNAL chain (never the Node-bound screen chain) so an older
    // journal resync cannot land after this fresher one; errors propagate to
    // the machine. A stalled /screen read cannot delay this reopen.
    await this.chainReconcile(instanceId, async () => {
      if (client) {
        await this.openFollowSocket(instance, client, client.appliedSeq);
        await client.resumeAfterReconnect();
        // This follow just did the authoritative catch-up the drained send was
        // waiting on; its debt is honoured. Fold the journal now. A throw from
        // either step skips the delete and rejects the job: resumeConnection's
        // catch (gate 6 item 2) runs the REST fallback for the still-owed row.
        this.reconcileOwed.delete(instanceId);
        this.settleFromJournal(instanceId, this.state.events[instanceId] ?? []);
      } else {
        // A fresh follow mounts AND catches the journal up; reaching here means
        // it certified — its debt is honoured too.
        await this.follow(instanceId);
        this.reconcileOwed.delete(instanceId);
        this.settleFromJournal(instanceId, this.state.events[instanceId] ?? []);
      }
    });
  }

  /** Foreground trigger used by Shell/PhoneShell (replaces raw catchup). */
  resumeActive(instanceId: Id | null) {
    if (instanceId) {
      const j = this.state.instances.find((i) => i.id === instanceId)?.journalId;
      if (j) this.bindConnection(instanceId, j);
      else {
        this.connectionBoundTo = instanceId;
        this.connectionBoundJournal = null;
      }
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
      // Pick the instance locks from FRESH durable rows, not the load-time
      // cache: another tab can persist a row after this tab loaded, and a
      // cache-only enumeration would never even attempt its instance lock.
      const now = Date.now();
      let durable;
      try {
        durable = await box.refreshDurable();
      } catch (err) {
        // flushAllOutbox is fire-and-forget from many call sites; a rejected
        // durable refresh must not become an unhandled rejection (gate 6 item
        // 4c). Surface it and stop this pass — the retry timer / reconnect /
        // next trigger re-runs the flush under the same command ids.
        this.reconcileToast(err, "刷新待发队列");
        return;
      }
      const instances = [
        ...new Set(durable.filter((r) => isDeliverableOutbox(r, now)).map((r) => r.instanceId)),
      ];
      // Rows owned in flight by another (possibly crashed) tab are not
      // deliverable until their lease expires; arm a wakeup at the earliest
      // expiry so this tab delivers them without an unrelated UI event.
      this.scheduleLeaseExpiryWakeup(durable, now, box.ownerId);
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
   * Deliver the first POSTable row of one lock turn, re-reading each mode from
   * the box so a just-landed steer promotion takes effect. Runs inside the
   * single-deliverer lock.
   *
   * A genuinely gone/terminal candidate (retracted/deleted by another tab, a
   * terminal done, an exhausted retry window) is SKIPPED and the bounded drain
   * continues to the NEXT candidate — returning null for it used to strand
   * every later queued row of the instance with no retry timer.
   *
   * An ABORTED inflight CLAIM is different (gate 6 item 4b): the row is still
   * first and still deliverable, but storage aborted or a foreign live lease
   * owns it. Skipping it would POST a LATER row first and break FIFO. That
   * result stops the whole turn (no POST at all this turn); the retry timer /
   * next flush retries the first row under its same commandId.
   *
   * Returns the POSTed row's commandId (added to the flush's attempted set)
   * or null when no candidate POSTed.
   */
  private async deliverOneRow(instanceId: Id, deliverable: OutboxRecord[]): Promise<Id | null> {
    const box = this.outbox;
    if (!box) return null;
    for (const rec of deliverable) {
      const fresh = box.get(rec.commandId) ?? rec;
      if (!isDeliverableOutbox(fresh, Date.now())) continue;
      const outcome = await this.deliverOutboxRecord(fresh);
      if (outcome === "claim-aborted") {
        // FIFO: the first deliverable row could not claim; never POST a later
        // row ahead of it. End the turn; the row keeps its place and retries.
        return null;
      }
      if (!outcome) continue;
      // If that drained the instance, run the post-delivery resync/screen chain
      // exactly once.
      if (!box.pendingFor(instanceId).length) {
        const drainJournalId =
          this.state.instances.find((i) => i.id === instanceId)?.journalId ?? null;
        await this.chainReconcile(instanceId, async () => {
          // Decide ownership WHEN THIS JOB RUNS, not when the drain enqueued
          // it: a live→recovering flip while we waited behind an earlier slow
          // catch-up must be observed here (fix-5 item 2). If THIS instance's
          // follow currently owns recovery, its reopenFollow resync is
          // authoritative — record the debt and skip the redundant REST read
          // (which would otherwise queue ahead of the socket reopen past the
          // 20 s watchdog). Otherwise run the bounded REST catch-up now.
          if (this.followOwnsCatchupNow(instanceId, drainJournalId)) {
            this.recordOwed(instanceId);
          } else {
            const client = drainJournalId ? this.journals.get(drainJournalId) : undefined;
            try {
              await client?.resumeAfterReconnect();
              // Successful catch-up honours any earlier debt this instance
              // carried (e.g. a drained row from before it was rebound away).
              this.reconcileOwed.delete(instanceId);
            } catch {
              // The REST catch-up failed while no follow owns recovery: stamp
              // convergence debt so the next failure/rebind/navigation trigger
              // retries instead of dropping it.
              this.recordOwed(instanceId);
            }
          }
          this.settleFromJournal(instanceId, this.state.events[instanceId] ?? []);
        });
        // The screen refresh rides the list scheduler (single-flight +
        // NODE_BUSY back-off) with COALESCING: a slow /screen while sends keep
        // completing can accumulate at most one active read plus one pending
        // follow-up per instance. runScreenRead still enters through the screen
        // chain, so every actual read waits behind the journal barrier above.
        this.kickScreenRefresh(instanceId);
      }
      return fresh.commandId;
    }
    return null;
  }

  /**
   * Classify a Hub command record into an outbox outcome.
   *  - rejected: settled with settlement.outcome rejected (real Node reject).
   *  - held: queued and never forwarded (Node offline); same-id re-POST later.
   *  - reconciling: FORWARDED to the Node (transport-written / native-
   *    acknowledged) but the Hub has no verdict yet — resolution "reconciling"
   *    OR "unknown". The Hub records the forward intent BEFORE awaiting the
   *    Node, so a lost POST can read the row back via GET as
   *    queued+forwarded+unknown; it is NOT proof of acceptance. Settled by a
   *    bounded GET (never re-forwarded).
   *  - sent: accepted/settled-completed or a clear forwarded row; reached the
   *    Hub/Node, await the journal join (never re-POST).
   */
  private classifyCommandResult(command: Command): "sent" | "held" | "reconciling" | "rejected" {
    return classifyCommandLike({
      state: command.state,
      resolution: command.resolution,
      settlement: command.settlement,
      // The POST envelope's rich dispatch states are the mapped form of the
      // ledger's `forwarded` flag (see api.ts mapCommand).
      forwarded: command.dispatch === "transport-written" || command.dispatch === "native-acknowledged",
    });
  }

  /** Classify the bare GET ledger record (its forward flag is `forwarded`). */
  private classifyCommandRecord(
    record: components["schemas"]["CommandRecord"],
  ): "sent" | "held" | "reconciling" | "rejected" {
    return classifyCommandLike({
      state: record.state,
      resolution: record.resolution,
      settlement: record.settlement
        ? {
            outcome: record.settlement.outcome as CommandSettlementOutcome,
            ...(record.settlement.reason ? { reason: record.settlement.reason } : {}),
          }
        : undefined,
      forwarded: record.forwarded,
    });
  }

  private async reconcileCommandViaGet(
    instanceId: Id,
    commandId: Id,
  ): Promise<"sent" | "held" | "reconciling" | "rejected" | null> {
    try {
      const record = await api.instanceCommandStatus(instanceId, commandId);
      return this.classifyCommandRecord(record);
    } catch {
      return null;
    }
  }

  /**
   * Bounded reconciliation of a forwarded-but-unresolved row (resolution
   * "reconciling" or "unknown"): poll the GET endpoint until it is
   * accepted/settled (→ sent), rejected, or the deadline passes (→ unknown).
   * NEVER re-POSTs — the Hub already forwarded the command. Runs OUTSIDE the
   * single-deliverer lock (it can take the whole 30 s deadline and must not
   * block another row of the instance, e.g. a steer); such a row is
   * non-deliverable, so no other owner can POST it meanwhile.
   */
  private reconcileReconcilingRow(instanceId: Id, commandId: Id): void {
    // One bounded GET loop per row per page lifetime: a row can be handed here
    // by both the POST landing and an outbox restore only across a real reload
    // (different page), but never run two loops concurrently in one page.
    if (this.reconcilingInFlight.has(commandId)) return;
    this.reconcilingInFlight.add(commandId);
    void this.runReconcileReconcilingRow(instanceId, commandId).finally(() => {
      this.reconcilingInFlight.delete(commandId);
    });
  }

  private readonly reconcilingInFlight = new Set<Id>();

  private async runReconcileReconcilingRow(instanceId: Id, commandId: Id): Promise<void> {
    const deadline = Date.now() + RECONCILE_GET_DEADLINE_MS;
    for (;;) {
      // Journal evidence is authoritative acceptance (the Node ran the
      // command): the live/catch-up settleFromJournal already retired the row
      // to "done". Stop polling and never downgrade it with a GET verdict.
      if (this.outbox?.get(commandId)?.state === "done") return;
      const verdict = await this.reconcileCommandViaGet(instanceId, commandId);
      // The follow frame may have retired the row to done WHILE this GET was
      // in flight (the top-of-loop check cannot see that): re-read before
      // every write so a late sent/held/unknown verdict never downgrades done.
      if (verdict === "rejected") {
        const result = await api.instanceCommandStatus(instanceId, commandId).catch(() => null);
        if (!(await this.reconcilePatch(commandId, {
          state: "rejected",
          serverState: result?.state,
          gotResponse: true,
          lastError: result?.settlement?.reason ?? "rejected by node",
        }))) {
          return;
        }
        return;
      }
      if (verdict === "sent") {
        await this.reconcilePatch(commandId, { state: "sent", gotResponse: true });
        return;
      }
      if (verdict === "held") {
        // Host went offline mid-reconcile: fall back to held retry (refund the
        // attempt and arm the bounded same-id re-POST).
        const wrote = await this.reconcilePatch(commandId, {
          state: "held",
          gotResponse: true,
          attempts: this.outbox?.get(commandId)?.attempts ?? 0,
        });
        if (wrote) this.scheduleHeldRetry(instanceId);
        return;
      }
      if (Date.now() >= deadline) {
        await this.reconcilePatch(commandId, { state: "unknown", lastError: "reconciliation deadline" });
        return;
      }
      await new Promise((r) => setTimeout(r, RECONCILE_GET_INTERVAL_MS));
    }
  }

  /**
   * Reconciliation-write gate: journal evidence ("done") is terminal and can
   * land while a GET is in flight, while the deadline fires, or in another
   * tab. The authoritative check lives INSIDE the patch transaction
   * ({@link Outbox.patch} re-reads the stored row): a cached non-done read
   * can still lose to a done whose write is in flight. Returns false when the
   * stored row was already done and the patch was dropped — the caller must
   * stop (no held retry, no POST) in that case.
   */
  private async reconcilePatch(commandId: Id, patch: Partial<OutboxRecord>): Promise<boolean> {
    if (this.outbox?.get(commandId)?.state === "done") return false;
    const rec = await this.safePatch(commandId, patch);
    // null = storage error or a row removed concurrently: the pre-tx behavior
    // (caller may retry on its own envelope) is unchanged; only a ROW that the
    // transaction read back as terminal done proves the patch was preserved.
    if (rec && rec.state === "done" && patch.state !== "done") return false;
    return true;
  }

  /** Storage-first patch that never throws into the delivery flow. */
  private async safePatch(commandId: Id, patch: Partial<OutboxRecord>): Promise<OutboxRecord | null> {
    try {
      return (await this.outbox?.patch(commandId, patch)) ?? null;
    } catch {
      /* storage failure: the durable inflight/lease state is the recovery path */
      return null;
    } finally {
      this.syncBubbleFromOutbox(commandId);
    }
  }

  /**
   * Deliver ONE authoritative durable row (handed in by the single-deliverer
   * lock). Claims a durable inflight lease before POSTing; a storage abort on
   * that claim means no POST and the row stays deliverable. "held" retries do
   * not spend the attempt budget; a queued-forwarded reconciling row is
   * settled by GET under a bounded deadline.
   *
   * Returns true when a POST ran, false when the row is gone/terminal (safe to
   * skip), or `"claim-aborted"` when the first row still exists but could not
   * be claimed this turn (storage abort, foreign live lease) — the caller must
   * preserve FIFO and not POST a later row first (gate 6 item 4b).
   */
  private async deliverOutboxRecord(
    current0: OutboxRecord,
  ): Promise<boolean | "claim-aborted"> {
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
    // in-memory marker. The same transaction that writes preserves a
    // journal-confirmed done: a stored done also means NO POST (the command
    // demonstrably executed; the Hub replay path would only be dead traffic).
    const isFreshAttempt = current0.state !== "held";
    let claimed;
    try {
      claimed = await box.patch(commandId, {
        state: "inflight",
        attempts: isFreshAttempt ? current0.attempts + 1 : current0.attempts,
        lease: { owner: box.ownerId, until: Date.now() + LEASE_TTL_MS },
      });
    } catch {
      // Storage aborted the claim: the first deliverable row is still first
      // and deliverable. Signal FIFO stop so a later row is not POSTed ahead.
      return "claim-aborted";
    }
    // A null claim means the row vanished inside the claim transaction
    // (another tab's retract deleted it — mergeUnlessDone resolves null
    // for a missing row): genuinely gone, safe to skip past.
    if (!claimed) return false;
    // Refuse a claim that did not come back with THIS tab's lease owner: a
    // foreign live lease is POSTing (or will POST) the row. Keep FIFO — do not
    // send a later row first this turn.
    if (claimed.lease?.owner !== box.ownerId) return "claim-aborted";
    // A journal-confirmed terminal done: the command already executed.
    if (claimed.state === "done") return false;
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
      // The rejection reason rides along on the rejected row so 未送达 can
      // show the Node/Hub's own neutral explanation inline.
      ...(r.state === "rejected" && r.lastError ? { outboxError: r.lastError } : {}),
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
        // The Promise.all above only resolves after a SUCCESSFUL
        // interaction-list read (an empty page is a valid success): arrival
        // watchers may take their baseline from this page.
        interactionsHydrated: true,
      });
      profileRegion("store.pollHydrate", () => {
        this.hydrateEffortEffective(instances.items);
        this.hydrateUsageRollups(instances.items);
        this.hydrateModels(instances.items);
        this.hydratePermissionEffective(instances.items);
      });
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
    this.connectionBindGen = 0;
    this.reconcileOwed.clear();
    this.owedReconcileRunning.clear();
    this.followReadyState.clear();
    this.followFrameAt.clear();
    for (const [, t] of this.outboxRetryTimer) clearTimeout(t);
    this.outboxRetryTimer.clear();
    this.outboxRetryAttempt.clear();
    for (const [, t] of this.heldRetryTimer) clearTimeout(t);
    this.heldRetryTimer.clear();
    this.heldRetryAttempt.clear();
    for (const [, t] of this.leaseWakeupTimer) clearTimeout(t);
    this.leaseWakeupTimer.clear();
    this.leaseWakeupUntil.clear();
    if (this.unloadClearTimer) {
      clearTimeout(this.unloadClearTimer);
      this.unloadClearTimer = null;
    }
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
    this.settledInteractions.clear();
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
      interactionsHydrated: false,
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
    profileRegion("store.hostsMerge", () => {
      const hosts = mergeHostWorkspaces(page.items, this.state.hosts);
      if (sameArrayIdentity(hosts, this.state.hosts)) return;
      // mapWorkspace rebuilds every row object; reuse the prior mapped
      // workspace when content is equal so buildSpaces inputs stay stable.
      const prior = new Map(this.state.workspaces.map((w) => [`${w.hostId}|${w.id}`, w]));
      let changed = false;
      const workspaces = hosts.flatMap((host) =>
        (host.workspaces ?? []).map((row) => {
          const mapped = mapWorkspace(row);
          const old = prior.get(`${mapped.hostId}|${mapped.id}`);
          if (!old) {
            changed = true;
          } else if (!structuralEqual(old, mapped)) {
            changed = true;
            return mapped;
          }
          return old ?? mapped;
        }),
      );
      changed ||= workspaces.length !== this.state.workspaces.length;
      this.emit(changed ? { hosts, workspaces } : { hosts });
    });
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
      // An older in-flight poll that returns a still-pending row must not
      // revert a locally committed review; merge with the same seq/outstanding
      // pin discipline as the instance list.
      profileRegion("store.pollMerge", () => {
        // The merges (including the structuralEqual identity checks) are timed
        // inside this region, not just the emit — the r1 probe boundary wrongly
        // measured only the latter (c-perffu r2 item 5).
        const nextInteractions = mergeInteractionSnapshots(
          interactions,
          this.state.interactions,
          this.settledInteractions,
          reqSeq,
          this.listOutstanding,
        );
        const nextInstances = mergeInstanceSnapshots(
          instances.items,
          this.state.instances,
          this.pinnedCreates,
          reqSeq,
          this.listOutstanding,
        );
        // An unchanged poll preserves every row identity (see the merge
        // functions), so skip the emission — and every consumer render wave —
        // entirely when neither snapshot changed. The hydration flag is
        // main's offline-reload baseline: a failed bootstrap leaves it false,
        // so the first SUCCESSFUL poll must still emit to flip it even when
        // both lists are empty/identical.
        if (
          !this.state.interactionsHydrated ||
          !sameArrayIdentity(nextInstances, this.state.instances) ||
          !sameArrayIdentity(nextInteractions, this.state.interactions)
        ) {
          this.emit({
            instances: nextInstances,
            interactions: nextInteractions,
            // A successful poll/refresh read settles the baseline even when the
            // bootstrap fetch failed (offline reload): an empty page here is a
            // valid success.
            interactionsHydrated: true,
          });
        }
      });
      // A response newer than a pin proves the server has spoken after the
      // create/answer. Combined with the in-flight sweep below (every older
      // request answered), that is when dropping the pin on a missing id is
      // safe. For an answered interaction, omission from the pending-only
      // list is the normal settled response, as is a non-pending row.
      for (const pin of this.pinnedCreates.values()) {
        if (reqSeq > pin.seq) pin.confirmedByNewer = true;
      }
      for (const [id, pin] of this.settledInteractions) {
        if (reqSeq <= pin.seq) continue;
        const row = interactions.find((candidate) => candidate.id === id);
        if (!row || row.state !== "pending") pin.confirmedByNewer = true;
      }
      profileRegion("store.pollHydrate", () => {
        this.hydrateEffortEffective(instances.items);
        this.hydrateUsageRollups(instances.items);
        this.hydrateModels(instances.items);
        this.hydratePermissionEffective(instances.items);
      });
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
        this.reapConfirmedPins(this.pinnedCreates);
        this.reapConfirmedPins(this.settledInteractions);
      }
    }
  }

  /**
   * Drop pins a strictly newer response has confirmed once no request at or
   * before the pin's seq is still outstanding: then no stale response can
   * resurrect what the newer page omitted.
   */
  private reapConfirmedPins(pins: Map<Id, { seq: number; confirmedByNewer: boolean }>) {
    for (const [id, pin] of pins) {
      if (!pin.confirmedByNewer) continue;
      let olderInFlight = false;
      for (const seq of this.listOutstanding) {
        if (seq <= pin.seq) {
          olderInFlight = true;
          break;
        }
      }
      if (!olderInFlight) pins.delete(id);
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
    // This mounted session owns the connection machine BEFORE the socket is
    // opened: if the subscribe fails while the Hub is unreachable (offline
    // reload), the machine's reconnect resume must still know which instance
    // to reopen — binding only after a successful open left reloaded sessions
    // with a delivered row but no follow and no journal join.
    this.connectionBoundTo = instance.id;
    this.connectionBoundJournal = instance.journalId;
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
          journalFloors: { ...this.state.journalFloors, [instanceId]: client.retainedFloorSeq },
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
    // The seed rows crossed the events READ, not the follow socket: register
    // them so a descending load-earlier window overlapping the seed counts as
    // duplicate-only (advancing the cursor) rather than a false prepend.
    client.noteHistory(history);
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
    this.emit({ journalFloors: { ...this.state.journalFloors, [instanceId]: client.retainedFloorSeq } });
    await this.openFollowSocket(instance, client, last, {
      earliestRetainedSeq: seed.windowFromSeq ?? "1",
      complete: seed.reachedAfterSeq,
    });
    // (connectionBoundTo/Journal were bound at the top of follow(), so the
    // machine owns this session even when this first open failed offline.)
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
    // The active session's link claim starts here: the machine requires a
    // frame within LIVE_FRAME_MS of the socket opening (a silent open must not
    // keep 已连接 over a frozen transcript). The subscribe snapshot/any event
    // dispatches {frame} and satisfies it. Only the CURRENTLY BOUND mount may
    // arm the claim: a superseded mount's socket opening late must not
    // start/reset the bind deadline for the new binding.
    if (this.connectionBoundJournal === instance.journalId) this.connection?.followBound();
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
          // Record the frame on its OWN journal; only the currently bound
          // session's frames drive the global machine. A socket left streaming
          // for a session the user navigated away from must not certify the
          // active session's link.
          this.followFrameAt.set(instance.journalId, Date.now());
          if (this.connectionBoundJournal === instance.journalId) {
            this.connection?.dispatch({ type: "frame" });
          }
        },
        onOpen: () => {
          // readyState is read live through getReadyState on every liveness
          // check; onOpen needs no cached copy.
        },
        // Genuine remote close of the CURRENT socket (eventsSubscribe ignores
        // the close of a socket it intentionally replaced — see api.ts). Only
        // the bound session's close is a global link event.
        onClose: () => {
          this.followReadyState.delete(instance.journalId);
          this.followFrameAt.delete(instance.journalId);
          if (this.connectionBoundJournal === instance.journalId) {
            this.connection?.dispatch({ type: "close" });
          }
        },
      },
    );
    this.subs.set(instance.journalId, sub.subscriptionId);
    // Hold the subscription's OWN live probe, never a sampled copy: a socket
    // that dies silently (no close callback, e.g. iOS background expiry) must
    // be seen as non-OPEN at the next liveness check.
    this.followReadyState.set(instance.journalId, sub.getReadyState);
    // The subscribe SNAPSHOT is the reopen + catch-up certificate: the server
    // answered over this exact socket. Count it as a frame so a resume
    // certifies live even for an idle session with no subsequent events.
    if (sub.getReadyState() === 1) {
      this.followFrameAt.set(instance.journalId, Date.now());
      if (this.connectionBoundJournal === instance.journalId) {
        this.connection?.dispatch({ type: "frame" });
      }
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

  /**
   * Fetch one window of older history for the transcript's load-earlier row.
   * Returns whether rows were prepended and whether the retained end was
   * reached (null when the instance/journal is not followed here).
   */
  async loadEarlier(instanceId: Id) {
    const instance = this.state.instances.find((i) => i.id === instanceId);
    if (!instance) return null;
    const client = this.journals.get(instance.journalId);
    if (!client) return null;
    const result = await client.loadEarlier();
    // Duplicate-only pages fire no onPrepend, so emit the (possibly advanced)
    // retained floor here: the button must stay visible while history remains.
    this.emit({ journalFloors: { ...this.state.journalFloors, [instanceId]: client.retainedFloorSeq } });
    return result;
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
    this.bindConnection(instanceId, instance.journalId);
    if (this.connection) {
      const client = this.journals.get(instance.journalId);
      // The 只读 banner Retry (and foreground catch-up) must heal a journal
      // whose backfill failed even when the follow socket is OPEN and freshly
      // framed: in that state the machine trusts the link and its resume is a
      // no-op, so without this forced REST catch-up the Retry button did
      // nothing. A non-live link is handed to the machine, whose resume
      // reopens the socket AND runs resumeAfterReconnect.
      if (client && client.status !== "live" && this.followSocketLive()) {
        await this.forceJournalCatchup(instanceId, client);
        return;
      }
      // resumeConnection reopens the socket + resyncs + flushes the outbox.
      this.connection.dispatch({ type: "resume" });
      return;
    }
    await this.catchupManual(instanceId);
  }

  /**
   * Force a bounded REST catch-up/backfill for an incomplete journal
   * (readonly-stale/gap-backfill) over an otherwise healthy socket. Runs on
   * the per-instance reconcile chain; a failed read leaves the journal at its
   * retryable status (the banner keeps its Retry action), never a false live.
   */
  private async forceJournalCatchup(instanceId: Id, client: JournalClient) {
    client.markReconnecting();
    try {
      await this.chainReconcile(instanceId, () => client.resumeAfterReconnect());
    } catch (err) {
      this.reconcileToast(err, "会话同步");
    }
    try {
      await this.refresh();
    } catch (err) {
      this.reconcileToast(err, "会话列表刷新");
    }
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
    const clientRequestId = localId();

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
    if (mode === "steer") {
      // A live steer is an interrupt: the 已打断 receipt must mean the
      // interrupt was actually accepted, so await the SAME single-deliverer
      // delivery steerHeld uses instead of resolving before the POST settles.
      // True only on authoritative acceptance (sent/done); held (host
      // offline), reconciling, rejected, unknown, or a lock/lease failure
      // report false even though the durable row keeps retrying.
      try {
        await this.outbox.withInstanceLock(instanceId, (iid, rows) =>
          this.deliverOneRow(iid, rows),
        );
      } catch {
        return false;
      }
      this.syncBubbleFromOutbox(commandId);
      const finalState = this.outbox.get(commandId)?.state;
      return finalState === "sent" || finalState === "done";
    }
    // Online ordinary send: flush now in the background; the POST never
    // blocks the caller.
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
    const clientRequestId = localId();
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
   *
   * This chain is JOURNAL-ONLY: it must never carry a Node-bound /screen read,
   * because the follow reopen (resumeConnection) joins it and a read stalled on
   * a saturated Node would park socket recovery (c-reconnfu). Screen reads use
   * {@link screenChain}.
   */
  private reconcileChain = new Map<Id, Promise<unknown>>();
  /**
   * Per-instance serial chain for Node-bound /screen reads. A screen read
   * waits behind any journal-chain work already queued for the instance (so it
   * cannot overtake a fresher catch-up — the read's start-time basis still
   * rejects any stale result), but journal/link work NEVER waits behind a
   * screen read. Screen reads also serialise among themselves so the list-poll
   * fan-out does not stack duplicates for one instance.
   */
  private screenChain = new Map<Id, Promise<unknown>>();
  /** Per-instance timer for retrying sends that failed transiently while live. */
  private outboxRetryTimer = new Map<Id, ReturnType<typeof setTimeout>>();
  private outboxRetryAttempt = new Map<Id, number>();
  /** Bounded retry timer for Hub-held (host offline) rows; no attempt cost. */
  private heldRetryTimer = new Map<Id, ReturnType<typeof setTimeout>>();
  private heldRetryAttempt = new Map<Id, number>();
  /**
   * Wakeup armed at another tab's in-flight lease expiry. A row another tab
   * owns in flight is non-deliverable until the lease passes; if that tab
   * crashed, nothing else re-triggers a flush here, so the wakeup fires
   * exactly when the row becomes stealable.
   */
  private leaseWakeupTimer = new Map<Id, ReturnType<typeof setTimeout>>();
  /** Expiry the per-instance wakeup is currently armed for (move-earlier). */
  private leaseWakeupUntil = new Map<Id, number>();
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

  /**
   * Arm one wakeup per instance at the earliest FOREIGN in-flight lease
   * expiry. Rows with no live foreign lease need nothing (they are
   * deliverable now or owned by this tab). An already-OVERDUE foreign lease
   * is skipped on the normal flush path (that same flush pass is about to
   * deliver the stealable row; arming an immediate timer would race the
   * delivery and chain further immediate flushes on a "held" answer). The
   * read-only cancelled-beforeunload re-arm passes includeOverdue: the
   * prompt can sit open past the deadline, and without an immediate arm the
   * stealable row would stay stranded. The immediate delivery is guarded
   * (a real pagehide clears the timer; flush checks the unload flag).
   * An already-armed wakeup is moved EARLIER when a freshly seen foreign
   * lease expires sooner (a second tab's newer in-flight row can carry a
   * shorter lease); it is never pushed out.
   */
  private scheduleLeaseExpiryWakeup(
    durable: OutboxRecord[],
    now: number,
    owner: Id,
    includeOverdue = false,
  ) {
    const earliest = new Map<Id, number>();
    for (const r of durable) {
      if (r.state !== "inflight" || !r.lease) continue;
      if (r.lease.owner === owner) continue;
      if (!includeOverdue && r.lease.until <= now) continue;
      const prev = earliest.get(r.instanceId);
      if (prev === undefined || r.lease.until < prev) earliest.set(r.instanceId, r.lease.until);
    }
    for (const [instanceId, until] of earliest) {
      const armed = this.leaseWakeupUntil.get(instanceId);
      if (armed !== undefined && armed <= until) continue;
      const old = this.leaseWakeupTimer.get(instanceId);
      if (old) clearTimeout(old);
      this.leaseWakeupUntil.set(instanceId, until);
      const timer = setTimeout(() => {
        this.leaseWakeupTimer.delete(instanceId);
        this.leaseWakeupUntil.delete(instanceId);
        void this.flushAllOutbox();
      }, Math.max(0, until - Date.now()));
      this.leaseWakeupTimer.set(instanceId, timer);
    }
  }

  private chainReconcile(instanceId: Id, job: () => Promise<unknown>): Promise<unknown> {
    const prev = this.reconcileChain.get(instanceId) ?? Promise.resolve();
    const next = prev.then(job, job);
    this.reconcileChain.set(instanceId, next);
    void next
      .finally(() => {
        if (this.reconcileChain.get(instanceId) === next) this.reconcileChain.delete(instanceId);
      })
      // The returned `next` already carries the rejection to its awaiter; the
      // finally-derived promise needs its own handler so a failed job (e.g. a
      // seed/subscribe failure) is not an unhandled rejection on top.
      .catch(() => undefined);
    return next;
  }

  /**
   * Serialise a Node-bound /screen read: it runs after the previously queued
   * screen read AND after any journal-chain reconciliation already queued at
   * enqueue time (so a screen read never overtakes a fresher journal resync),
   * but it never joins the journal chain itself — a stalled read must not block
   * the follow reopen. Journal work queued AFTER the read starts is not waited
   * for: the read's start-time basis in {@link readAndCommitScreen} rejects a
   * result that a newer journal frame supersedes.
   */
  private chainScreenRead(instanceId: Id, job: () => Promise<unknown>): Promise<unknown> {
    const journalGate = this.reconcileChain.get(instanceId) ?? Promise.resolve();
    const prev = this.screenChain.get(instanceId) ?? Promise.resolve();
    const gate = Promise.all([journalGate.catch(() => undefined), prev.catch(() => undefined)]).then(
      () => undefined,
    );
    const next = gate.then(job);
    this.screenChain.set(instanceId, next);
    void next
      .finally(() => {
        if (this.screenChain.get(instanceId) === next) this.screenChain.delete(instanceId);
      })
      .catch(() => undefined);
    return next;
  }

  async refreshScreen(
    instanceId: Id,
    opts: { chained?: boolean } = {},
  ): Promise<void> {
    // The 2.5 s list-poll fan-out reads OUTSIDE any user flow: it must not
    // overtake a journal catch-up/resync already queued for the instance, which
    // can carry an unseen HIGHER-seq screen the RPC's start-time basis could
    // not know about. Run it on the SCREEN chain: it waits behind queued
    // journal work but a stalled Node read cannot block the journal/link chain.
    // Chain-internal callers (the post-delivery resync) call readAndCommitScreen
    // themselves: nesting a chain entry inside its own job would deadlock.
    if (opts.chained) {
      await this.chainScreenRead(instanceId, () => this.readAndCommitScreen(instanceId));
      return;
    }
    await this.readAndCommitScreen(instanceId);
  }

  private async readAndCommitScreen(instanceId: Id): Promise<void> {
    // Ordering vs catch-up: the list-poll path enters here ON the per-instance
    // screen chain (refreshScreen({chained:true})), which gates on journal work
    // queued first, so a queued catch-up carrying an unseen higher-seq screen
    // always applies before this read starts; the basis is captured at
    // execution. The generation guard plus the basis (from committed screens,
    // screen observations and known events) additionally covers a live frame
    // arriving during the read itself.
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
      this.admitScreenRead(id);
    }
  }

  /**
   * Post-delivery screen refresh: the coalescing counterpart of the list
   * poll's {@link refreshScreens}. Every drained delivery used to append
   * another serialized screen job, so while /screen was slow (a saturated Node
   * parks the RPC for its REST timeout + NODE_BUSY back-off) pending screen
   * work grew one entry per send. Instead:
   *
   * - no read active/queued and not backed off → admit one now;
   * - a read is ACTIVE → collapse the demand to the single
   *   {@link screenCoalesced} flag: any number of sends yields ONE follow-up,
   *   re-armed in the read's completion callback;
   * - already pump-queued (not started) → nothing: its start-time basis is
   *   captured at execution and already covers the just-settled send;
   * - inside a NODE_BUSY back-off window → nothing: the back-off timer's own
   *   {@link refreshScreens} re-arm is the follow-up.
   *
   * The admitted read still runs via {@link chainScreenRead}, so the journal
   * barrier (no overtaking a fresher resync) stands for every actual read.
   */
  private kickScreenRefresh(instanceId: Id) {
    if (Date.now() < (this.screenBackoffUntil.get(instanceId) ?? 0)) return;
    if (this.screenActive.has(instanceId)) {
      this.screenCoalesced.add(instanceId);
      return;
    }
    if (this.screenPending.has(instanceId)) return;
    this.admitScreenRead(instanceId);
  }

  /**
   * Admit one row into the scheduler queue (single-flight guard + lifecycle
   * skip) and pump. Shared by the list poll and the post-delivery kick.
   */
  private admitScreenRead(id: Id) {
    const instance = this.state.instances.find((row) => row.id === id);
    if (!instance || SCREEN_SKIP_LIFECYCLES.has(instance.lifecycle)) return;
    this.screenPending.add(id);
    this.screenQueue.push(id);
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
      this.screenActive.add(id);
      void this
        .runScreenRead(id)
        .catch(() => undefined)
        .finally(() => {
          this.screenInFlight -= 1;
          this.screenActive.delete(id);
          this.screenPending.delete(id);
          // Realise the single coalesced post-delivery demand as exactly ONE
          // follow-up read. A NODE_BUSY answer armed screenBackoffUntil plus a
          // retry timer; that timer's refreshScreens is the follow-up, so the
          // busy Node is not hammered immediately.
          if (this.screenCoalesced.delete(id) && Date.now() >= (this.screenBackoffUntil.get(id) ?? 0)) {
            this.admitScreenRead(id);
          }
          this.pumpScreenReads();
        });
    }
  }

  private async runScreenRead(instanceId: Id) {
    try {
      // The list-poll fan-out is ordered behind any in-flight journal
      // reconciliation for this instance.
      await this.refreshScreen(instanceId, { chained: true });
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
          // Optimistic entry before the configure is journaled: no request
          // timestamp yet; the queued lifecycle event stamps it on arrival.
          requestObservedAt: this.state.effortPending[instanceId]?.requestObservedAt ?? null,
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
    // Pin the settlement against the list seq captured NOW, before the
    // follow-up refresh increments it: every list response at or before this
    // seq may predate the commit. The pin makes the committed card immune to
    // a stale pending copy AND to an empty newer page followed by that stale
    // poll (the merge keeps a tombstone until a newer page and the last older
    // request have both resolved).
    this.settledInteractions.set(interactionId, {
      seq: this.listReqSeq,
      confirmedByNewer: false,
    });
    this.markInteractionCommitted(interactionId);
    try {
      await this.refresh();
      const interaction = this.state.interactions.find((i) => i.id === interactionId);
      if (interaction) await this.catchup(interaction.instanceId);
    } catch {
      // Post-commit sync failure: the tombstone settles the card
      // optimistically; the next authoritative list confirms it.
    }
    // Settle the local projection even if the authoritative refresh failed
    // or removed the row: the 200 receipt is the commit. The pin keeps any
    // delayed pending poll from re-enabling approve/deny; it is released in
    // refresh() once a newer list omits the id (the normal pending-only
    // response) or reports a non-pending state.
    this.markInteractionCommitted(interactionId);
    // Clear the answering marker: the local guard keeps any delayed pending
    // poll from re-enabling approve/deny even after the marker is gone.
    {
      const { [interactionId]: _removed, ...rest } = this.state.answering;
      this.emit({ answering: rest });
    }
  }

  /** Flip a still-pending local interaction row to answer-committed. */
  private markInteractionCommitted(interactionId: Id) {
    if (
      !this.state.interactions.some((row) => row.id === interactionId && row.state === "pending")
    ) {
      return;
    }
    this.emit({
      interactions: this.state.interactions.map((row) =>
        row.id === interactionId
          ? { ...row, state: "answer-committed" as const }
          : row,
      ),
    });
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

  /** c-composerpop e2e seam: inject a Hub-computed usage rollup exactly as a
   *  poll hydration would have folded it in (fake-node sessions never report
   *  usage, so the mobile stacked-sheet cases inject one). Inert unless the
   *  e2e seam marker is set — no production call site can reach it. */
  setUsageRollupForTest(instanceId: Id, rollup: UsageRollup) {
    if (!e2eSeamsEnabled()) return;
    this.emit({ usageRollup: { ...this.state.usageRollup, [instanceId]: rollup } });
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
