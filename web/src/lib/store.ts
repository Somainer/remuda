import { useSyncExternalStore } from "react";
import { profileRegion } from "./profileFlags";
import { structuralEqual } from "./structuralEqual";

/** Bounded journal tail the list reads per live instance to project its phrase. */
const SUMMARY_TAIL = 64;
import type { Command, CommandSettlementOutcome } from "../types/command";
import type { components } from "./api.generated";
import type { Host, Instance } from "../types/instance";
import type { Interaction, InteractionAnswer } from "../types/interaction";
import type { InteractionResolutionReason } from "../types/generated";
import type { Observation } from "../types/observation";
import type { Id, U64 } from "../types/wire";
import type { PromptMode } from "../types/generated";
import type { Workspace, WorkspaceSnapshot } from "../types/workspace";
import type { AttachmentRef } from "./attachments";

/**
 * c-reconnfu gate 35 DELIVERY TOKEN: identity captured once at the start of a
 * send / lock turn / fallback and validated synchronously before any POST.
 */
type DeliveryToken = {
  box: Outbox;
  authGen: number;
  transitionSeq: number;
};
/**
 * c-reconnfu gate 37 item 1: the auth generation + transition sequence a
 * credential transition captured when it ACQUIRED the chain. Carried in the
 * transition outcome so the PUBLIC continuation can re-validate synchronous
 * current ownership before bootstrapping (a peer logout bumps the generation
 * without touching the sequence).
 *
 * Gate 38 adds `ext`: the external-identity epoch captured when the
 * transition was ISSUED. A logout/401/peer change while the credential
 * request (or a queued successor) is suspended moves it; every externally
 * visible effect of the continuation is then suppressed silently.
 */
type AuthTransitionOwner = {
  gen: number;
  seq: number;
  ext: number;
};
/**
 * c-reconnfu gate 43 — TRI-STATE bootstrap / connection install outcome.
 *  - "installed":   a current-identity connection machine exists (newly built
 *                   or adopted) and the authenticated/degraded UI is settled;
 *                   callers are fulfilled.
 *  - "obsolete":    the external epoch moved OR a same-epoch credential
 *                   transition superseded this run's generation. This run NEVER
 *                   fulfils a caller itself: while extEpoch is still current
 *                   callers are handed to the CURRENT generation's install
 *                   (join its in-flight run or start one); after an external
 *                   change they settle silently as retired.
 *  - "unavailable": the CURRENT generation's durable storage genuinely failed
 *                   to open/read: publish the settled degraded/offline
 *                   cached-session state (ready: true, authed per session)
 *                   with polling/recovery and fulfil — never a loading screen.
 */
type BootstrapOutcome = "installed" | "obsolete" | "unavailable";
/**
 * Outcome of a serialized credential transition run. `obsolete` (c-reconnfu
 * gate 40 item 1) means an EXTERNAL identity change retired the transition
 * WHILE IT WAS QUEUED (after it issued but before it acquired the chain): it
 * never sent its credential request, never changed ownership, and the public
 * caller settles silently (resolves, never throws — nothing was attempted).
 */
type AuthTransitionOutcome<T> =
  | { obsolete: true }
  | {
      obsolete: false;
      superseded: boolean;
      ok: true;
      result: T;
      acquired: AuthTransitionOwner;
    }
  | {
      obsolete: false;
      superseded: boolean;
      ok: false;
      error: unknown;
      acquired: AuthTransitionOwner;
    };
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
  CLAUDE_XHIGH_INDEX,
  DEFAULT_EFFORT_INDEX,
  coupledSelection,
  effortAt,
  effortFromRecord,
  claudeVersionGate,
  effortWireName,
  mapEffort,
  type ClaudeVersionGate,
  type EffortAxis,
  type EffortKind,
  type EffortSelection,
  type ModelEffortCatalogRow,
} from "../features/session/effort";
import {
  effectiveFromObservation,
  effectiveFromRecord,
  effortFlagSettles,
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
import { hostClaudeVersion } from "./driverMatrix";
import { e2eSeamsEnabled } from "./e2eSeams";
import { JournalClient, type JournalRead } from "./journal";
import { id, localId, now } from "./ids";
import { mockGappedTail, mockJournalIds } from "./mock";
import { readDeviceSettings } from "../features/settings/prefs";
import {
  MOCK_BOOTSTRAP_TOKEN,
  clearCachedSession,
  clearSession,
  dropDeviceCookie,
  readLoggedOut,
  readSession,
  writeSession,
  type DeviceSession,
  type PairCode,
  type PairedDevice,
} from "./session";

/**
 * A push-down in flight (chip shows 切换中 / 排队中 until BOTH requested axes
 * settle). D-056: the request is two axes — level `low…max` and the
 * orthogonal ultracode flag — each confirmed by its own read-back.
 */
export type EffortPending = {
  /** Per-instance nonce identifying one switch; stale lifecycles are dropped. */
  nonce: number;
  /**
   * Per-request id (c-effortread r10 items 1–4 / r19 item 1) minted for THIS
   * switch and carried web → Hub → Node → driver. Lifecycle edges and the
   * requested echo settle/clear this pending ONLY when they carry the same
   * id. An id-less projection settles NOTHING — a missing id is not
   * legacy-driver evidence — so an unknown request otherwise waits for its
   * bounded unconfirmed outcome.
   */
  requestId: string;
  /** Axis this switch moves ("flag" = orthogonal toggle, "tier" = level). */
  axis: EffortAxis;
  /** Requested native level word. */
  name: string;
  /** Requested switch state. */
  ultracode: boolean;
  /** True while the agent is working — it applies at the next idle. */
  queued: boolean;
  /** Wall-clock ms the pending state was entered (stale-state safety net). */
  at: number;
  /**
   * Effective `observedAt` of the newest projection BEFORE this request.
   * Records the pre-switch baseline this request was made against
   * (c-effortread r10 item 1); since r19 item 1 settlement is own-id only and
   * no longer compares read-back times against it, it is retained as
   * provenance metadata.
   */
  baselineObservedAt: string | null;
  /**
   * `observedAt` of THIS request's own `effort-queued` lifecycle; null until
   * that edge arrives. Kept separately from the pre-request baseline.
   */
  queuedObservedAt: string | null;
  /**
   * c-effortread r16 item 5: the JOURNAL SEQ of THIS request's own
   * `effort-queued` lifecycle (lifecycle-owned pendings only). Protocol
   * timestamps have millisecond resolution, so two queued lifecycles emitted
   * in the same millisecond tie on {@link queuedObservedAt}; the journal seq
   * is the total order that disambiguates them. Null on client-owned
   * pendings (their nonce/afterReplacement protection does not use this).
   */
  queuedSeq: string | null;
  /** Level axis confirmed by a newer projection. */
  levelSettled: boolean;
  /** Switch axis confirmed by a newer projection. */
  flagSettled: boolean;
  /**
   * Who owns this in-flight request: "client" when this tab posted the
   * configure (its axes are the nonce-identified request), "lifecycle" when
   * the only evidence is a wire `effort-queued` edge (configure posted outside
   * the UI). A mismatched degraded verdict is trusted as terminal only for a
   * wire-owned request; against a client request it is a replaced request's
   * stale verdict and is ignored (c-effortui r2 item 2).
   */
  source: "client" | "lifecycle";
  /**
   * c-effortread r13 item 1: this client request replaced an earlier
   * in-flight request (A→B→C). It records replacement provenance for the
   * request's nonce/debug state. Since r19 item 1 an ID-LESS edge settles
   * NOTHING regardless, so the word-ambiguity guard that used this flag to
   * reject a same-word queued bound is moot; the request fails closed on its
   * own id evidence until its bounded reconciliation ends it.
   */
  afterReplacement: boolean;
  /**
   * The exact command word the DRIVER types for this configure
   * (c-effortui r4 item 3). On decoupled builds a TIER move with the flag on
   * is a plain level word (`/effort max` leaves the ortho flag as-is), so its
   * `effort-queued/applied/degraded:<word>` lifecycle carries the LEVEL even
   * though the request's flag is true. Matching purely on
   * `{name,ultracode}` rejected that word (flag-on pending vs level word) and
   * stuck the indicator; the matcher compares the lifecycle word to this.
   * `null` for wire-owned pendings synthesized from the word itself.
   */
  commandWord: string | null;
  /**
   * c-effortread r18 item 2 / r19 item 1: the per-SESSION driver id-capability
   * captured for THIS request — "unknown" or "id-capable". It is a persisted
   * fact about the session's driver, carried across requests, NOT inferred
   * from this request's own first queued frame:
   *  - unknown: no own-id evidence yet (a fresh session, another client's
   *    id-less configure, or a poll that arrives before the first id-bearing
   *    frame). The request settles ONLY on its own id-bearing evidence, or via
   *    its bounded unconfirmed outcome — never from an id-less matching
   *    projection or an instance-list poll. An ID-LESS lifecycle is NOT legacy
   *    evidence: the current driver emits one whenever the incoming configure
   *    omitted its optional requestId (another client), so capability is
   *    unknown until proven.
   *  - id-capable: an edge carrying a real effort request id ECHOED BY THIS
   *    SESSION was seen. Monotonic: an id-less edge (an unowned verdict the
   *    mapper could not pair, provenance loss) never downgrades it.
   */
  idCapability: EffortDriverIdCapability;
  /**
   * c-effortread r13 item 2 / r14 item 2: wall-clock ms at which CURRENT
   * ACTIVITY EVIDENCE showed this request leaving the driver queue (the turn
   * becoming idle — directly, or via waiting-interaction on a
   * PermissionRequest/Elicitation before Stop/StopFailure). The bounded
   * reconciliation is anchored HERE, not at the immutable `queued` snapshot
   * taken at send time: a request queued behind a running turn gets its full
   * request-owned bound only AFTER the turn ends. Null while the turn is still
   * running; an idle-at-send request initializes it to the send time.
   */
  leftQueueAt: number | null;
};

/**
 * A refusal that keeps the ultracode switch disabled, projected from an
 * `effort-degraded` lifecycle. Model-scoped refusals clear on a model
 * change; process-scoped ones last the process (D-056 owner rule: this is a
 * configure OUTCOME, never a session end — retry is allowed, not blocked).
 */
export type UltraRefusal = {
  reason: string;
  scope: "model" | "process";
  modelId: string | null;
  at: string;
};

/**
 * c-effortread r18 item 2 / r19 item 1: per-SESSION knowledge of whether the
 * driver handling the session echoes the configure's per-request effort id on
 * its lifecycle/verdict frames.
 *  - unknown: no own-id evidence yet. Fail closed: only own-id evidence
 *    settles. An id-less lifecycle is NOT legacy evidence — the current driver
 *    emits one whenever the configure omitted its optional requestId (another
 *    client) — so it leaves the capability unchanged.
 *  - id-capable: an own-id echo was seen; monotonic, never downgraded by
 *    later id-less evidence.
 */
export type EffortDriverIdCapability = "unknown" | "id-capable";

/** Parse of an effort lifecycle status word into its two axes. */
type EffortLifecycleRequest = {
  name?: string;
  ultracode?: boolean;
};

/** Stable reason codes that keep the switch disabled (D-056 §5). */
const ULTRA_MODEL_REASON = "ultracode-unavailable-for-model";
const ULTRA_WORKFLOWS_REASON = "ultracode-workflows-disabled";

/**
 * Native-lifecycle `relatedIds` key carrying the effort configure's
 * per-request id (c-effortread r10 items 1–4); mirrors
 * `remuda_protocol::EFFORT_REQUEST_ID_KEY`.
 */
const EFFORT_REQUEST_ID_KEY = "effortRequestId";

/** Pending entries older than this without a verdict are dropped. */
const EFFORT_PENDING_MAX_AGE_MS = 30 * 60_000;

/**
 * c-effortread r12 item 5 / r13 item 4: bounded, request-owned reconciliation
 * for an ID-LESS driver. An older driver that accepts a no-op (the target
 * already equals the observed state) emits no completion id, and its mapper
 * suppresses unchanged observations — without this bound the chip sat on
 * 切换中 for the whole 30-minute max age.
 *
 * The bound mirrors the driver's own read-back window (10s) plus a delivery
 * margin. It NEVER queries the driver for current state: the protocol has no
 * such operation (r13 item 4 coordinator ruling), so a re-read of the stored
 * Hub record would be stale-state theatre, not fresh evidence. A genuine
 * post-command projection instead arrives through the normal channels (a live
 * follow effort edge, the routine list poll) and settles through the strict
 * fallback before the deadline; otherwise the request ends in the explicit
 * "unconfirmed" outcome — never a silent pending.
 */
const EFFORT_UNCONFIRMED_BOUND_MS = 15_000;

/**
 * c-effortread r15 item 1: whether current activity evidence says a queued
 * effort request is STILL waiting to leave the driver queue. The driver
 * executes a queued switch only once the agent is Idle/Done AND
 * interactive_ready — a session parked on a PermissionRequest/Elicitation
 * reports `waiting-interaction` and is just as unrunnable as a `working`
 * one (crates/remuda-driver/src/pty_interaction.rs, effort.rs queue worker).
 * Treating waiting-interaction as idle initialised the execution deadline
 * immediately, so a request could end "unconfirmed" while the driver was
 * still waiting to type it. Only authoritative idle evidence anchors the
 * deadline (see {@link HubStore.emit}'s queue-exit fold).
 */
function effortAwaitsQueueExit(activity: Instance["activity"] | undefined): boolean {
  return (
    activity?.state === "known" &&
    (activity.value === "working" || activity.value === "waiting-interaction")
  );
}

/**
 * c-effortread r15 item 3 / r16 item 5: whether an incoming `effort-queued`
 * lifecycle edge is provably NEWER than the lifecycle-owned pending's own
 * queued edge. The primary order is the millisecond observedAt; protocol
 * timestamps have millisecond resolution (crates/remuda-protocol scalar
 * Timestamp), so two edges emitted within one millisecond tie and fall back
 * to the journal SEQ — the hub's total order (later seq wins, an older/equal
 * seq never replaces). Callers must have validated both timestamps finite.
 */
function lifecycleEdgeIsNewer(
  incomingMs: number,
  incomingSeq: string,
  establishedMs: number,
  establishedSeq: string,
): boolean {
  if (incomingMs > establishedMs) return true;
  if (incomingMs < establishedMs) return false;
  try {
    return BigInt(incomingSeq) > BigInt(establishedSeq);
  } catch {
    return false;
  }
}

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

/**
 * Predict the EXACT word the driver types for a live configure
 * (c-effortui r4 item 3; mirrors remuda-driver `EffortRequest::for_configure`).
 * The driver journals the typed command word in its queued/applied/degraded
 * lifecycle; on decoupled builds a TIER move that carries the flag
 * (`{max,true}`) is typed as the plain level (`/effort max` leaves the ortho
 * flag untouched), so a flag-ON pending can legitimately carry a level
 * lifecycle word. Same observed tier + flag change → `ultracode on|off`;
 * a tier move → the level word; coupled flag-on → `ultracode`.
 */
function predictDriverCommandWord(
  effort: EffortSelection,
  axis: EffortAxis,
  instance: Instance | undefined,
  state: HubState,
  instanceId: Id,
): string {
  const kind = instance?.kind ?? "claude";
  if (kind !== "claude") return effort.name;
  // c-effortui r5 item 3: mirror effortVersionGate's carrier lock — a
  // shell-pty driver never types the ultracode words; its predictor is always
  // the plain level (the shell-pty carrier fails flag-on configure itself).
  if (instance?.driver === "shell-pty") return effort.name;
  // Snapshot reported version first, then the host's pinned CLI (the store's
  // documented source order), mirroring HubStore.effortVersionGate.
  const hostId =
    instance?.hostId ??
    state.instances.find((row) => row.id === instanceId)?.hostId;
  const host = state.hosts.find((h) => h.id === hostId);
  const reported = instance?.capabilities?.binaryVersion;
  const gate = claudeVersionGate(reported ?? hostClaudeVersion(host) ?? "");
  if (gate === "coupled" && effort.ultracode) return "ultracode";
  if (gate === "decoupled") {
    // c-effortread r9 item 6: an explicit FLAG axis is the orthogonal toggle
    // even at a cold start (no proven tier needed) — the driver executes
    // exactly that word. An explicit TIER axis is the plain level. Only a
    // legacy request (no axis) falls back to same-tier provenance.
    if (axis === "flag") {
      return effort.ultracode ? "ultracode on" : "ultracode off";
    }
    if (axis === "tier") return effort.name;
    // c-effortui r5 item 2: mirror the driver's provenance fallback —
    // observed effective, optimistic selection, then the durable instance
    // record (effortName/effortUltracode) — so a session opened on another
    // device / created outside this tab predicts the same word the driver
    // types (driver: bridge.observed() ?? requested()).
    const currentLevel =
      state.effortEffective[instanceId]?.name ??
      state.effort[instanceId]?.name ??
      instance?.effortName ??
      null;
    if (currentLevel === effort.name) {
      return effort.ultracode ? "ultracode on" : "ultracode off";
    }
  }
  return effort.name;
}

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
 * Map a lifecycle word onto the two request axes. The driver (D-056) journals
 * the level word (`max`), the coupled bare word (`ultracode`), or the
 * decoupled toggle word (`ultracode on` / `ultracode off` — space form, the
 * actual command; legacy `:`/`-` separators accepted too). Only axes the word
 * actually names are returned, so a stale lifecycle for a replaced request
 * can never match the new one.
 */
function effortLifecycleRequest(word: string): EffortLifecycleRequest {
  // Normalize the toggle spelling: "ultracode on/off" with any single
  // separator (the command itself uses a space) → "ultracode:on|off".
  const normalized = word.trim().toLowerCase().replace(/^ultracode[\s:_-]+(on|off)$/, "ultracode:$1");
  // c-effortread r15 item 2: the BARE word is the coupled build's xhigh-tier
  // command WITH the flag (D-056, driver `EffortRequest::from_level`), not a
  // flag-only toggle. Returning only {ultracode:true} targeted the default
  // high tier and a fresh {xhigh, on} read-back could never settle it. The
  // normalized "ultracode:on" (the decoupled toggle) moves the flag only.
  if (normalized === "ultracode") {
    return { name: "xhigh", ultracode: true };
  }
  if (normalized === "ultracode:on") {
    return { ultracode: true };
  }
  if (normalized === "ultracode:off") {
    return { ultracode: false };
  }
  return { name: normalized };
}

/**
 * Deduplicate an emitted lifecycle edge by request nonce when the wire
 * carries one (c-effortread r9 item 8). The driver/fake-node may echo the
 * client nonce on configure statuses; until one is plumbed through this
 * matcher is axis-value based.
 */
type EffortLifecycleKind = "queued" | "applied" | "degraded";

/** Whether a lifecycle word is an ultracode switch word (bare or toggle).
 *  c-effortui r5 item 2: accept any separator the driver journals — the
 *  actual command space form (`ultracode on`), plus `:`/`_`/`-`. */
function lifecycleNamesUltraWord(word: string): boolean {
  const n = word.trim().toLowerCase();
  return n === "ultracode" || /^ultracode[\s:_-]+(on|off)$/.test(n);
}

/**
 * Whether a parsed lifecycle word refers to the in-flight request.
 *
 * c-effortread r9 item 8: queued/applied must match the request's EXACT
 * identity — comparing the parsed ON/OFF flag value and tier against the
 * pending request. A delayed `ultracode off` applied for a replaced request A
 * must never delete newer request B's `ultracode on` indicator (the old
 * matcher accepted it because B carried ultracode:true). Only genuine
 * ultracode REFUSALS/degradations get the looser carrying-word matching,
 * since refusal evidence is process-local, not request-scoped.
 */
function effortLifecycleMatches(
  pending: EffortPending,
  word: string,
  kind: EffortLifecycleKind,
): boolean {
  const normalized = word.trim().toLowerCase();
  // A REFUSAL/DEGRADATION names the ultracode SWITCH and may concern a flag
  // carried on a plain tier command: keep the looser carrying-word rule for
  // it only. The outer handler additionally treats refusal evidence as
  // process-local and must not clear a non-matching pending via this result.
  if (kind === "degraded") {
    if (
      pending.commandWord !== null &&
      (normalized === pending.commandWord ||
        (pending.commandWord.startsWith("ultracode ") && normalized === "ultracode") ||
        (pending.ultracode && lifecycleNamesUltraWord(normalized)))
    ) {
      return true;
    }
  } else if (pending.commandWord !== null) {
    // Exact predicted command word only — no flag-carries cross-matching.
    return normalized === pending.commandWord;
  }
  // Wire-owned / synthesized pending (no client predicted word): compare the
  // parsed ON/OFF VALUE and tier against the request identity.
  const request = effortLifecycleRequest(word);
  const bareCoupled = normalizedWordIsBareUltracode(word);
  if (request.name !== undefined) {
    if (request.name !== pending.name) return false;
    // A plain level word asserts nothing about the flag, so it cannot
    // identify a pending that moves the flag — the bare coupled word
    // asserts BOTH axes (r15 item 2), so it is exempt.
    if (request.ultracode === undefined && pending.ultracode && !bareCoupled) {
      return false;
    }
  }
  if (request.ultracode !== undefined) {
    if (request.ultracode !== pending.ultracode) return false;
    if (bareCoupled && pending.name !== "xhigh") return false;
  }
  return request.name !== undefined || request.ultracode !== undefined;
}

function normalizedWordIsBareUltracode(word: string): boolean {
  return word.trim().toLowerCase() === "ultracode";
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
  /** D-056 §5 refusal that keeps the ultracode switch disabled. */
  effortRefusal: Record<string, UltraRefusal>;
  /** Per-instance nonces so a replaced request can't be settled by stale events. */
  effortNonces: Record<string, number>;
  /** GET /v1/supply/catalog rows (per-model default + ultracode capability). */
  supplyCatalog: ModelEffortCatalogRow[] | null;
  /** r7 item 4(a) / c-effortread r8 item 1: per-instance read-back WITHDRAWAL
   *  WATERMARK — the parsed observedAt of the latest `readbackAvailable:false`
   *  edge (or Hub projection level→null). Any level view at or OLDER than the
   *  watermark is rejected (a "Load earlier" older edge cannot restore a
   *  withdrawn level); only a strictly newer valid view clears it. Absent =
   *  not withdrawn. Distinct from "never read back": a pending push-down
   *  renders `?` only while a watermark exists, never merely because no edge
   *  arrived. */
  effortReadbackWithdrawn: Record<string, string>;
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
  effortRefusal: {},
  effortNonces: {},
  supplyCatalog: null,
  effortReadbackWithdrawn: {},
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
/** c-cardsettle r2 item 4: exported for the settlement-race unit test. */
export function mergeInteractionSnapshots(
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
   * c-effortread r9 e2e seam only: instanceId → resolvers notified once the
   * follow socket has delivered at least one contiguous live batch PAST the
   * bounded seed cursor (so a configure posted afterwards cannot be absorbed
   * into the snapshot as replay). Populated solely by
   * `waitForFollowCaughtUpForTest`, which no-ops unless the e2e marker is set.
   */
  private followCaughtUp = new Map<Id, Set<(seq: string) => void>>();
  private followCaughtUpSeq = new Map<Id, string>();
  /**
   * §9.1: instanceId → effective `observedAt` of a push-down the durable Hub
   * projection settled before its live follow frame arrived. The matching live
   * observation must STILL be treated as our own push-down (leave the slider
   * where the user put it so a clamp mismatch stays visible), not as a
   * terminal-side switch that would fold the slider to the clamped level.
   */
  private settledEffortPushdown = new Map<Id, string>();
  /**
   * c-effortread r18 item 2 / r19 item 1: instanceId → this session driver's
   * known id-capability, persisted ACROSS requests. Absent is "unknown". Only
   * an edge carrying a real own-id echo upgrades the session to "id-capable",
   * monotonically. An id-less lifecycle proves NOTHING (the current driver
   * emits one whenever the configure omitted its optional requestId — e.g.
   * another client), so it can never mark, settle, or downgrade the session:
   * unknown requests fail closed on their own id evidence or their bounded
   * unconfirmed path.
   */
  private effortDriverId = new Map<Id, EffortDriverIdCapability>();

  /** The session driver's known id-capability ("unknown" until own-id proof). */
  effortIdCapability(instanceId: Id): EffortDriverIdCapability {
    return this.effortDriverId.get(instanceId) ?? "unknown";
  }

  /**
   * Record one piece of per-session driver id evidence. Only an edge carrying
   * a real effort request id proves the driver id-capable (monotonic); an
   * id-less edge is ignored. Returns the resulting capability.
   */
  private noteEffortDriverId(
    instanceId: Id,
    carriesId: boolean,
  ): EffortDriverIdCapability {
    if (carriesId) this.effortDriverId.set(instanceId, "id-capable");
    return this.effortDriverId.get(instanceId) ?? "unknown";
  }
  /**
   * The effortEffective each instance carried on the LAST Hub poll (null =
   * polled but absent, absent from the map = never polled). r6 item 6: a poll
   * withdraws the live projection only when a previously seen HUB record
   * becomes null — an unrelated lifecycle write that advances updatedAt while
   * the Hub never projected effort must not make the chip render "?".
   */
  private polledEffort = new Map<Id, EffortEffectiveView | null>();
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
  /**
   * c-reconnfu gate 41 S1: the external epoch under which the CURRENT
   * connection machine was installed (null after teardown). initConnection
   * uses it to adopt a machine a concurrent same-epoch bootstrap already
   * installed instead of constructing a second one.
   */
  private connectionInstallEpoch: number | null = null;
  private bootstrapInFlight: { ext: number; gen: number; promise: Promise<BootstrapOutcome> } | null = null;
  private outbox: Outbox | null = null;
  private outboxUnsubscribe: (() => void) | null = null;
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
  private outboxInit: Promise<"ok" | "obsolete" | "unavailable"> | null = null;
  /**
   * Auth-identity generation (c-reconnfu gate 33 item 2). Bumped by EVERY
   * session mutation via applyAuthIdentityChange (login, passkey login,
   * logout, bootstrap-401 clear, peer-tab session change). An Outbox
   * initializer captures the generation when it starts; if the generation has
   * moved by the time it would install its box, the initialization is
   * obsolete — it disposes what it built and installs/flushes NOTHING, so a
   * cross-identity initializer can never publish an outbox (or run its
   * post-load flush) belonging to the previous identity.
   */
  private authGen = 0;
  /**
   * EXTERNAL-identity epoch (c-reconnfu gate 38/39, the ONE mechanism for auth
   * work suspended across an external change). Incremented ONLY by an identity
   * change originating OUTSIDE this tab's serialized credential chain — local
   * logout, bootstrap's 401 session clear, and a peer-tab login/logout
   * observed via storage. Ordinary credential succession (login B after login
   * A in this tab) does NOT bump it.
   *
   * The complete rule:
   *  R-a: every PUBLIC auth entry point (login, pair, passkeyLogin, bootstrap,
   *       any other that leads to session/resources) captures extEpoch at
   *       ENTRY, BEFORE its first await, and carries that original value
   *       through transitions and settlement.
   *  R-b: every internal helper that installs resources or publishes identity
   *       (initConnection, ensureOutbox, startOutboxInit, connection-machine
   *       creation, row restore, session write/publish, authed publication)
   *       takes the caller's epoch as a PARAMETER and validates it
   *       synchronously immediately before each install/publish; a retired
   *       caller gets an obsolete result. No helper captures a fresh epoch on
   *       behalf of a caller.
   *  R-c: an EXTERNAL change always leaves the UI SETTLED — the external
   *       application of this change publishes ready:true, authed:false,
   *       session:null synchronously; retired continuations then stay silent.
   *
   * A mismatch anywhere means the identity is gone — stop silently,
   * publish/install nothing more, settle the caller's promise as
   * superseded/failed, never hang.
   */
  private extEpoch = 0;
  /**
   * Monotonic id of the current/most-recent credential transition
   * (c-reconnfu gate 35). Incremented when a transition ACQUIRES ownership
   * (inside the serialized chain). A delivery token captures the seq at turn
   * start; tokenLive fails once a newer transition exists.
   */
  private transitionSeq = 0;
  /**
   * Monotonic id assigned SYNCHRONOUSLY when a credential transition is issued
   * (before chain acquisition), so two transitions issued back-to-back before
   * either acquires still get distinct ids (c-reconnfu gate 36 item 4). Chain
   * acquisition order equals issuance order; the id doubles as
   * {@link transitionSeq} at acquisition.
   */
  private transitionIssuedSeq = 0;
  /**
   * Pending-bootstrap obligation on the serialized transition chain
   * (c-reconnfu gate 36 item 4). A successful login whose own bootstrap is
   * skipped because a newer transition was issued sets this; the FINAL owner
   * of the chain discharges it when it settles — including on FAILURE, when an
   * earlier transition's valid session is on disk and would otherwise never
   * reach the authenticated UI/connection (a failed queued login exits before
   * bootstrap).
   *
   * Gate 37 item 1: the obligation is bound to the auth GENERATION that owns
   * it (re-bound on every credential-transition acquisition = explicit
   * transfer along the chain) and is invalidated by applyAuthIdentityChange,
   * so an EXTERNAL identity change (logout, bootstrap 401, peer-tab logout)
   * between acquisition and settlement makes a stale continuation discharge
   * nothing — even though readSession() may still return a cached session.
   */
  private authBootstrapOwed: { gen: number } | null = null;
  /**
   * DELIVERY TOKEN (c-reconnfu gate 35). Captured ONCE at the start of every
   * send / lock turn / fallback and never re-read inside a delivery. The
   * synchronous validator {@link tokenLive} confirms the continuation is still
   * on the same box, auth generation and transition sequence with no open
   * transition; a stale token produces a "stale" result that ends the whole
   * lock turn (never "skip and continue") so a continuation can never POST
   * under a replacement identity.
   */
  private deliveryToken(box: Outbox): DeliveryToken {
    return { box, authGen: this.authGen, transitionSeq: this.transitionSeq };
  }
  private tokenLive(t: DeliveryToken): boolean {
    return (
      t.box === this.outbox &&
      !t.box.isDisposed() &&
      t.authGen === this.authGen &&
      t.transitionSeq === this.transitionSeq &&
      this.authTransition === null
    );
  }
  /**
   * Fallback (storage-unavailable) token check (c-reconnfu gate 35 item 5):
   * there is no box to compare, so validate only the generation/transition
   * captured before the async availability check.
   */
  private fallbackTokenLive(gen: number, seq: number, wasOpen: boolean): boolean {
    return gen === this.authGen && seq === this.transitionSeq && !wasOpen && this.authTransition === null;
  }
  /**
   * Auth-transition blocker (c-reconnfu gate 34 Fence A). While a credential
   * request is in flight (its response sets the new cookie in HEADERS before
   * the body parses), outbox initialization/delivery must be held so the old
   * box cannot retry with the new cookie. runAuthTransition wraps every
   * login/passkey call: it disposes BEFORE the request, blocks ensureOutbox
   * until the body settles (success or failure), and then releases so a
   * current-generation box initializes.
   */
  private authTransition: Promise<void> | null = null;
  private resolveAuthTransition: (() => void) | null = null;

  /**
   * Lazily open the durable outbox (also used outside bootstrap, e.g. a
   * send that races first mount or unit-test callers). Resolves false when no
   * storage backend is writable — send then uses the legacy direct path.
   */
  /**
   * The ONE helper through which every auth-identity mutation must pass
   * (c-reconnfu gate 33 item 1): login, passkey login, logout, bootstrap's
   * 401 session clear, and the peer-tab storage event. It disposes the
   * current Outbox (and invalidates any in-flight initialization) BEFORE the
   * new identity is used, and bumps {@link authGen} so an initializer that
   * started under the previous identity can never install/flush. Durable rows
   * stay in storage; the next ensureOutbox (under the new generation) loads a
   * fresh Outbox with a new lifetime.
   *
   * `external` (c-reconnfu gate 38): the change originated OUTSIDE this tab's
   * credential chain (local logout, bootstrap 401, peer-tab event). It ALSO
   * increments {@link extEpoch} and clears the in-memory session cache, so
   * auth work already suspended at an await (a login continuation, a running
   * bootstrap) can neither publish/install anything nor answer hasDeviceSession
   * from a stale cached session. Ordinary credential succession
   * (runAuthTransition acquisition, the mock first-login) passes nothing and
   * leaves the external epoch alone.
   */
  private applyAuthIdentityChange(external = false) {
    // Bump the auth generation and dispose the current outbox. A credential
    // transition ALSO bumps transitionSeq (in runAuthTransition); non-request
    // changes (logout/401/peer) bump the generation here so their generation
    // invalidates in-flight initializers even though no credential transition
    // blocker is open.
    this.authGen += 1;
    // Gate 37 item 1: an EXTERNAL identity change invalidates any pending
    // bootstrap obligation. A credential transition's own acquisition routes
    // through here too but RE-TRANSFERS a predecessor obligation explicitly
    // afterwards (see runAuthTransition), so this only kills obligations whose
    // identity is gone.
    this.authBootstrapOwed = null;
    if (external) {
      this.extEpoch += 1;
      // The peer tab removed persisted storage itself; also forget the cached
      // in-memory session in THIS tab so readSession()/hasDeviceSession()
      // cannot resurrect the old identity.
      clearCachedSession();
      // Gate 40 item 2: dispose the OLD identity's connection machine
      // coherently (socket/timers/follow bookkeeping). Retaining it while
      // publishing "offline" would strand a false-offline field — its live
      // frames suppress redundant publication and a healthy re-login's reuse
      // branch would then never resync; a fresh login builds a new machine
      // that publishes its own state. The logout/401 callers also dispose via
      // their own teardown, so this is idempotent for them.
      this.disposeConnectionForAuthChange();
      // Gate 39 R-c: leave the UI in a SETTLED state synchronously. A peer
      // logout observed while no entry point is running (or while one is
      // parked) must not strand protected pages on the loading screen waiting
      // on a bootstrap that will now return silently; the logout/401 callers
      // emit their own identical state right after this, so this is an
      // idempotent publish. With the machine just disposed, "offline" is the
      // truthful machine-less state.
      this.emit({
        ready: true,
        authed: false,
        session: null,
        connection: "offline",
      });
    }
    this.disposeOutboxForAuthChange();
  }

  /**
   * Serialized transition chain tail (c-reconnfu gate 35 item 2). Overlapping
   * credential requests cannot bypass Fence A.
   */
  private authTransitionChain: Promise<void> = Promise.resolve();
  /** Monotonic sequence shared by all transition acquisitions (c-reconnfu gate 35). */
  /**
   * FENCE A (c-reconnfu gate 34/35): wrap a credential-establishing request.
   * Serialized — a second login queues behind the first and only disposes/bumps
   * (its own seq) once it acquires the chain after the first settles. While a
   * transition is open, outbox initialization and delivery are blocked by the
   * delivery token + ensureOutbox. The blocker releases in finally on success,
   * rejection or body error; gate-35 item 1 then runs current-generation
   * initialization in that finalizer (when this transition still owns it).
   */
  /**
   * Run a serialized credential transition. Always RESOLVES (never throws)
   * with a discriminated outcome so the PUBLIC caller can settle the chain's
   * bootstrap obligation on BOTH success and failure (c-reconnfu gate 36
   * item 4):
   *  - `{ ok: true, result, superseded }` — the credential request succeeded.
   *  - `{ ok: false, error, superseded }` — it rejected; the caller rethrows
   *    `error` after dealing with any obligation.
   * `superseded` is true when a NEWER transition was issued before this one
   * finished: this caller must defer to the final owner of the chain rather
   * than run its own post-login bootstrap.
   */
  private async runAuthTransition<T>(
    issue: () => Promise<T>,
    // Gate 39 R-a: the EXTERNAL epoch captured by the PUBLIC entry point
    // BEFORE its first await (login/passkeyLogin entry), passed in here — the
    // helper never captures a fresh epoch on the caller's behalf. A logout/401/
    // peer change between the entry capture and settlement retires the whole
    // transition even when the interruption happened during a PRELIMINARY
    // await (passkey challenge/ceremony) before this wrapper was entered.
    extAtEntry: number,
  ): Promise<AuthTransitionOutcome<T>> {
    // Monotonic ISSUANCE order, assigned synchronously at call time: two
    // transitions issued back-to-back before either acquires still get
    // distinct ids, so the earlier one can tell at settle time that a newer
    // transition was issued and hand the chain's obligations to it
    // (c-reconnfu gate 36 item 4).
    const myIssuedSeq = ++this.transitionIssuedSeq;
    // Gate 38: a login can be in flight while the store is still logged out
    // (no outbox initialized yet), so ensure the peer watch is live BEFORE the
    // credential body can park — a peer logout during it must invalidate the
    // continuation even though startOutboxInit has not run.
    this.registerPeerAuthWatch();
    // Gate 38/39: the caller's entry-time external epoch governs this whole
    // transition; it is never refreshed inside the helper.
    const myExt = extAtEntry;
    // Chain acquisition order equals issuance order (the chain serializes in
    // call order), so the issued id is also this transition's ownership seq.
    // Link onto the chain and publish the tail BEFORE awaiting so the next
    // caller queues behind this run.
    const run = async (
      prev: Promise<void>,
    ): Promise<AuthTransitionOutcome<T>> => {
      await prev;
      // Gate 40 item 1: the FIRST thing after the chain opened is the epoch
      // check. A transition QUEUED behind its predecessor may have been
      // retired by an EXTERNAL logout/401/peer change while waiting. It must
      // return obsolete BEFORE acquiring ownership (no seq/generation change,
      // no disposal, no blocker) and, crucially, BEFORE invoking issue() — the
      // credential request must never reach the Hub (a passkey finish / pair
      // redeem would otherwise create a device session and Set-Cookie
      // server-side after this identity logged out). The predecessor already
      // released the blocker and nulled the obligation; the chain tail simply
      // resolves so a later fresh transition can acquire.
      if (this.extEpoch !== myExt) {
        return { obsolete: true };
      }
      this.transitionSeq = myIssuedSeq;
      // Gate 37 item 1: this transition's OWN acquisition routes through
      // applyAuthIdentityChange (which invalidates any obligation as an
      // external change would). A predecessor's obligation is explicitly
      // TRANSFERRED to this chain owner and re-bound to the new generation —
      // the only way an obligation survives an identity bump. Gate 38: the
      // transfer itself survives only while no EXTERNAL change happened before
      // this successor acquired.
      const carriedObligation = this.authBootstrapOwed;
      this.applyAuthIdentityChange();
      // Gate 36 item 3: capture the auth generation THIS transition owns at
      // ACQUISITION. A non-transition identity change (logout, bootstrap 401,
      // peer-tab session removal) bumps the generation while a queued/in-flight
      // credential request is still open, WITHOUT advancing transitionSeq;
      // that generation must invalidate this transition's recovery finalizer.
      const myGen = this.authGen;
      const acquired: AuthTransitionOwner = { gen: myGen, seq: myIssuedSeq, ext: myExt };
      if (carriedObligation && this.extEpoch === myExt) this.authBootstrapOwed = { gen: myGen };
      this.authTransition = new Promise<void>((resolve) => {
        this.resolveAuthTransition = resolve;
      });
      let result: T;
      let error: unknown;
      let failed = false;
      try {
        result = await issue();
      } catch (err) {
        failed = true;
        error = err;
      } finally {
        // Release the blocker. Run current-generation initialization in the
        // finalizer (gate 35 item 1) only while this transition still owns
        // BOTH the sequence and the generation it acquired — a queued newer
        // transition OR a logout/401/peer change invalidates it. Gate 38 adds
        // the external epoch: a transition issued before an external logout
        // never installs an outbox after it.
        const resolve = this.resolveAuthTransition;
        this.resolveAuthTransition = null;
        this.authTransition = null;
        resolve?.();
        const stillOwns =
          this.transitionSeq === myIssuedSeq && this.authGen === myGen && this.extEpoch === myExt;
        if (stillOwns) void this.ensureOutbox(myExt).catch(() => undefined);
      }
      // A NEWER transition was issued before this one settled (whether or not
      // it has acquired yet): this caller must not run the post-login
      // bootstrap — the final owner's continuation owns the chain's bootstrap
      // obligation (gate 36 item 4).
      const superseded = this.transitionIssuedSeq !== myIssuedSeq;
      if (failed) return { obsolete: false, ok: false, error, superseded, acquired };
      return { obsolete: false, ok: true, result: result!, superseded, acquired };
    };
    // Snapshot the previous chain SYNCHRONOUSLY and publish the new tail
    // before run's body microtask executes — so run awaits the PREVIOUS chain,
    // never itself, and the next caller queues onto this run.
    const prev = this.authTransitionChain;
    const resultPromise = run(prev);
    this.authTransitionChain = resultPromise.then(
      () => undefined,
      () => undefined,
    );
    return resultPromise;
  }


  /**
   * Settle the serialized chain's bootstrap obligation from a credential
   * transition's PUBLIC continuation (c-reconnfu gate 36 item 4) — the only
   * place a successful transition's session has just been written.
   *  - successful FINAL owner: discharge any pending obligation and bootstrap
   *    now;
   *  - successful SUPERSEDED transition: transfer the obligation to the
   *    chain's final owner instead of bootstrapping under it;
   *  - FAILED final owner: when a predecessor's valid session is on disk and
   *    still unbootstrapped, bootstrap THAT session (a queued login failing
   *    after an earlier one succeeded must not leave the app logged out);
   *  - failed superseded transition: nothing — the newer owner is responsible.
   *
   * Gate 37 item 1: every discharge SYNCHRONOUSLY re-validates that this
   * continuation still owns the auth generation AND transition sequence it
   * captured at acquisition, and that the obligation itself is bound to the
   * current generation. An EXTERNAL change between acquisition and settlement
   * (logout, bootstrap 401, a peer-tab logout that bumps authGen without
   * touching transitionSeq) nulls the obligation and moves the generation, so
   * a stale continuation never bootstraps — readSession()'s cached in-memory
   * session is consulted only AFTER those checks and cannot outlive the peer
   * logout for this decision.
   *
   * Gate 37 item 2: returns the bootstrap promise so the PUBLIC login
   * continuations can await it; a final owner's login resolves only once the
   * authenticated UI is published (AuthGate no longer bounces a too-early
   * navigation back to /login), while a superseded caller transfers the
   * obligation and returns immediately without waiting on its successor.
   *
   * Gate 38: the captured external epoch gates EVERY effect here. An external
   * logout/401/peer change after the transition was issued (or after the
   * successor acquired) makes the continuation return silently — no
   * obligation (re)created, no bootstrap — so a stale result is never re-bound
   * to the current generation and readSession() (its cache cleared) cannot
   * supply the old identity.
   */
  private async settleAuthBootstrap(outcome:
    | AuthTransitionOutcome<DeviceSession>,
  ): Promise<void> {
    // Gate 40 item 1: a transition retired while QUEUED sent no request and
    // never acquired — the external change already nulled any obligation.
    if (outcome.obsolete) return;
    const extLive = () => this.extEpoch === outcome.acquired.ext;
    const ownsNow = () =>
      this.authGen === outcome.acquired.gen && this.transitionSeq === outcome.acquired.seq;
    if (outcome.ok) {
      if (outcome.superseded) {
        // Explicit transfer to the chain's final owner. This continuation can
        // resume AFTER the successor already acquired (so this.authGen is the
        // successor's generation): bind the obligation to the generation that
        // is current NOW. Only through this VERIFIED succession — and never
        // after an external change, which nulls the obligation instead.
        if (!extLive()) return;
        this.authBootstrapOwed = { gen: this.authGen };
        // Gate 43 item 1: do NOT resolve this login while the installation is
        // still parked. Await the current generation's bootstrap (single-flight
        // joins the successor's run, or the tri-state runner starts one if the
        // successor later fails): this caller is fulfilled only once a machine
        // or settled degraded state exists for the current identity.
        await this.bootstrap().catch(() => undefined);
        return;
      }
      this.authBootstrapOwed = null;
      if (!extLive() || !ownsNow()) return;
      await this.bootstrap().catch(() => undefined);
      return;
    }
    // Failed final owner: discharge a predecessor's obligation ONLY while no
    // external change happened, this continuation is the current owner, the
    // obligation is bound to THIS generation, and a session really exists.
    if (
      !outcome.superseded &&
      extLive() &&
      ownsNow() &&
      this.authBootstrapOwed?.gen === this.authGen &&
      readSession()
    ) {
      this.authBootstrapOwed = null;
      await this.bootstrap().catch(() => undefined);
    }
  }

  /**
   * Dispose the current Outbox INSTANCE for an auth-identity change.
   * Idempotent. See {@link applyAuthIdentityChange} for the invariant.
   */
  private disposeOutboxForAuthChange() {
    this.outbox?.dispose();
    this.outbox = null;
    this.outboxUnsubscribe?.();
    this.outboxUnsubscribe = null;
    this.outboxInit = null;
    this.outboxDegraded = false;
  }

  /**
   * Tear down the connection machine and its follow bookkeeping for an auth
   * change (c-reconnfu gate 40 item 2). Disposing the machine coherently —
   * rather than retaining a live machine while publishing "offline" — is what
   * makes the R-c logged-out publication truthful: a subsequent healthy
   * login's bootstrap builds a FRESH machine whose onState publishes
   * recovering/live, so the banner can never stick on a stale false offline.
   * Idempotent.
   */
  private disposeConnectionForAuthChange() {
    // Gate 41 S1/S2: drop the install stamp with the machine so a later
    // same-epoch (impossible after an external epoch bump) or post-login
    // bootstrap builds a fresh machine rather than adopting the dead one.
    this.connection?.dispose();
    this.connection = null;
    this.connectionInstallEpoch = null;
    this.connectionBoundTo = null;
    this.connectionBoundJournal = null;
    this.connectionBindGen = 0;
    this.reconcileOwed.clear();
    this.owedReconcileRunning.clear();
    this.followReadyState.clear();
    this.followFrameAt.clear();
  }

  /**
   * Peer-tab auth-identity watcher (c-reconnfu gate 32 item 1b). localStorage
   * 'storage' events fire only in OTHER documents, so this observes a login/
   * logout/session replacement in another tab and disposes THIS tab's Outbox
   * BEFORE anything else runs — satisfying the invariant that an outbox never
   * survives an auth identity change. Registered once for the HubStore
   * lifetime (not per connection bootstrap).
   */
  private static readonly SESSION_STORAGE_KEYS = new Set([
    "runtime.device-session",
    "runtime.device-id",
    "runtime.logged-out",
  ]);
  private onPeerAuthChange = (event: StorageEvent) => {
    if (!event.key || !HubStore.SESSION_STORAGE_KEYS.has(event.key)) return;
    // A peer logged in/out or replaced the session: an EXTERNAL change — rotate
    // the auth generation AND external epoch (clearing this tab's cached
    // session), dispose the outbox, and invalidate suspended auth work
    // (gate 38). No reconnect here — the UI/next action decides re-auth.
    this.applyAuthIdentityChange(true);
  };
  private peerAuthWatchRegistered = false;
  private registerPeerAuthWatch() {
    if (this.peerAuthWatchRegistered) return;
    if (typeof window === "undefined" || typeof window.addEventListener !== "function") return;
    window.addEventListener("storage", this.onPeerAuthChange);
    this.peerAuthWatchRegistered = true;
  }

  /**
   * Result of an outbox availability check (c-reconnfu gate 34 item 1):
   *  - "ok": a current-generation Outbox is installed.
   *  - "obsolete": the initializer lost an auth-generation race — durable
   *    storage is fine and the CURRENT generation gets its own box; the caller
   *    must NOT treat this as "storage unavailable" and must never POST
   *    directly (no unkeyed duplicate); a durably committed command is left to
   *    the current-generation box's recovery.
   *  - "unavailable": durable storage genuinely could not be opened/written —
   *    the legacy direct POST may run, but ALWAYS carrying the client command
   *    id as its dedupe id.
   */
  private async ensureOutbox(ext: number): Promise<"ok" | "obsolete" | "unavailable"> {
    // Gate 39/40 R-b: `ext` is REQUIRED — the epoch the CALLER captured at ITS
    // public entry before its first await. This helper never adopts a fresh
    // epoch on a caller's behalf (no default). The current-identity test seam
    // is {@link ensureOutboxForTest}.
    const epoch = ext;
    // c-reconnfu gate 35 item 2: LOOP while any credential transition is open.
    // Overlapping logins are serialized: each release can be followed by a
    // queued transition acquiring, so re-await until the chain fully settles.
    // Gate 39 R-b: revalidate the caller's epoch at EVERY pass, including
    // immediately after the open-transition wait — a continuation parked here
    // across an external logout resumes "obsolete" and never starts an
    // initializer under the new identity.
    // eslint-disable-next-line no-constant-condition
    while (true) {
      if (this.extEpoch !== epoch) return "obsolete";
      if (this.outbox) return "ok";
      const open = this.authTransition;
      if (!open) break;
      // eslint-disable-next-line no-await-in-loop
      await open;
    }
    if (this.extEpoch !== epoch) return "obsolete";
    if (this.outbox) return "ok";
    return this.startOutboxInit(epoch);
  }

  private startOutboxInit(ext: number): Promise<"ok" | "obsolete" | "unavailable"> {
    // Gate 39 R-b: the CALLER's epoch is validated synchronously BEFORE any
    // install. A retired caller (an external change after its public entry)
    // gets "obsolete" — this helper never captures a fresh epoch for it.
    if (this.extEpoch !== ext) return Promise.resolve("obsolete");
    if (this.outboxInit) return this.outboxInit;
    this.registerPeerAuthWatch();
    // Capture the auth generation this initializer starts under (gate 33
    // item 2). It may complete after an auth change bumped authGen; in that
    // case it must not install the box it built nor run its post-load flush.
    // Gate 38/39: the CALLER's external epoch gates every install point; an
    // external change (logout/401/peer) retires the initializer even where the
    // generation accounting alone would still match.
    const gen = this.authGen;
    const obsolete = () => gen !== this.authGen || ext !== this.extEpoch;
    this.outboxInit = (async (): Promise<"ok" | "obsolete" | "unavailable"> => {
      try {
        // Test gate: hold the initializer mid-open so a test can change auth
        // identity before completion (gate 33 item 2 / gate 34 item 1).
        if (this.outboxOpenGateForTest) {
          await new Promise<void>((resolve) => this.outboxOpenGateForTest?.(resolve));
        }
        const opened = this.forceUnavailableStorageForTest
          ? await Promise.reject(new Error("test: storage unavailable"))
          : await openOutboxStorage();
        // Auth changed while the storage backend was opening: report OBSOLETE,
        // not unavailable — storage is fine, the current generation owns it.
        if (obsolete() || !this.outboxInit) {
          return "obsolete";
        }
        this.outboxDegraded = opened.degraded;
        // Test gate: hold between storage-open and Outbox.load (the second
        // obsolete-initializer suspension point, gate 34 item 5).
        if (this.outboxLoadGateForTest) {
          await new Promise<void>((resolve) => this.outboxLoadGateForTest?.(resolve));
        }
        const box = await Outbox.load(opened.storage);
        if (obsolete() || !this.outboxInit) {
          // Another identity won: dispose the box we just built and leave the
          // current outbox (if a newer initializer installed one) untouched.
          box.dispose();
          return "obsolete";
        }
        this.outbox = box;
        this.outboxUnsubscribe = box.subscribe(() => this.emit({ outboxPending: box.pendingCount() }));
        // Gate 11 item 5: a tracked lease-expiry wakeup inside Outbox reclaims
        // an inactive claim (kept for a cloned live tab / unknown ownership /
        // a failed load-time reclaim) and then triggers the normal
        // single-deliverer flush here — no unrelated UI event required.
        box.onClaimWake = () => {
          // A central lease/delivery wake is the trigger: run the GUARDED
          // flush, which skips rows currently HELD — their bounded
          // held-retry timer (armed by the same reconciliation via
          // onHeldLease) is the only thing allowed to re-POST them. A normal
          // user/online/held-retry flush still re-forwards held rows.
          void this.flushAllOutbox(new Set(), true);
        };
        // The central reconciliation hands HELD rows to their one allowed
        // clock: the bounded same-id held-retry timer, whether this tab
        // delivered the held answer or observed a sibling's held write.
        box.onHeldLease = (instanceId) => {
          if (!this.pageIsUnloading) this.scheduleHeldRetry(instanceId);
        };
        // Gate 21 item 3: persistent durable-read failures stop after the
        // bounded envelope instead of re-entering at attempt 0 forever —
        // surface the explicit state; a later manual/online flush, the
        // unload-cancel rearm or a reload restarts the chain.
        box.onReadRecoveryExhausted = () => {
          this.toast("待发队列连续读取或投递失败，已暂停自动重试；恢复后将在下次发送时继续");
        };
        this.emit({ outboxPending: box.pendingCount() });
        // Test gate (c-reconnfu gate 44 item 1): fires AFTER the box is installed
        // but before the initializer reports "ok", letting a regression
        // interleave a credential-generation supersession (which disposes the
        // just-installed box) exactly between the two — the window in which a
        // parked initConnection used to read "ok" with this.outbox null.
        if (this.outboxInstalledGateForTest) {
          await new Promise<void>((resolve) => this.outboxInstalledGateForTest?.(resolve));
        }
        if (opened.degraded) this.toast("本机无法可靠保存待发消息（存储不可用）");
        // Rows left inflight by a closed tab are due immediately — but never
        // from an initializer whose identity is already obsolete (it may have
        // been superseded by a newer one that owns the flush).
        if (!obsolete() && this.outbox === box) void this.flushAllOutbox();
        return "ok";
      } catch {
        if (obsolete()) return "obsolete";
        this.outboxDegraded = true;
        return "unavailable";
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
  /** c-cardsettle: global Hub settlement socket (instances ending → cards drop). */
  private stopSettlementFollow: (() => void) | null = null;
  /** Trailing coalescer so a burst of settlement notices triggers one refresh. */
  private settlementRefreshTimer: ReturnType<typeof setTimeout> | null = null;
  /** Dev/observability: how many settlement frames THIS store handled. */
  private settlementCount = 0;
  /**
   * c-effortread r12 item 5: per-instance bounded reconciliation timers for
   * client effort requests, keyed by instance and owned by their nonce. A
   * newer request replaces the slot (the old timer is cancelled); a settled
   * request's timer fires into a no-op.
   */
  private effortReconcileTimers = new Map<
    Id,
    { nonce: number; timer: ReturnType<typeof setTimeout> }
  >();

  subscribe = (listener: Listener) => {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  };

  getSnapshot = () => this.state;

  private emit(patch: Partial<HubState>) {
    // c-effortread r13 item 2/3, r14 item 2: current activity evidence (live
    // follow native turn events or full-instance entity snapshots) is the ONLY
    // authority on when a queued effort request can actually run. A pending
    // stamped while a turn was running waits for the next idle evidence; at
    // that instant the exact nonce-guarded pending gets its reconciliation
    // bound anchored (a client pending also drops the chip's 排队中 flag — its
    // immutable send-time snapshot could not track this; a wire pending keeps
    // the driver-provided queue word). Everything happens inside the same
    // emission so listeners never see split state.
    //
    // r14 item 2: the exit is ANY transition of authoritative activity TO
    // idle, not only a direct working → idle. The production turn end can be
    // working → waiting-interaction → idle (PermissionRequest/Elicitation
    // then Stop/StopFailure), and the previous activity may be unknown when a
    // fresh idle snapshot is the first evidence this store sees. Requiring
    // prev === "working" left a silent accepted legacy no-op pending until
    // the 30-minute max age; anchor the deadline whenever a nonce-owned
    // pending is still awaiting its queue exit and activity is now idle
    // (idle → idle emissions are no-ops: such a pending already carries its
    // anchor, and a request can only be queued while a turn was running).
    let queueExits: Map<Id, number> | null = null;
    if (patch.instances) {
      for (const next of patch.instances) {
        if (next.activity?.state !== "known" || next.activity.value !== "idle") continue;
        const pending = this.state.effortPending[next.id];
        if (pending && pending.leftQueueAt == null) {
          (queueExits ??= new Map()).set(next.id, pending.nonce);
        }
      }
    }
    let merged = patch;
    if (queueExits?.size) {
      const atExit = Date.now();
      const pendingNext = { ...(patch.effortPending ?? this.state.effortPending) };
      for (const [id, nonce] of queueExits) {
        const pending = pendingNext[id];
        if (!pending || pending.nonce !== nonce || pending.leftQueueAt != null) {
          queueExits.delete(id);
          continue;
        }
        pendingNext[id] = {
          ...pending,
          leftQueueAt: atExit,
          // Only the client's optimistic snapshot tracks the chip word from
          // local activity; a wire pending's queued state is driver-provided.
          ...(pending.source === "client" ? { queued: false } : {}),
        };
      }
      if (queueExits.size) merged = { ...patch, effortPending: pendingNext };
    }
    // c-effortread r17 item 2: later busy evidence (working or
    // waiting-interaction) — in the SAME follow batch as an earlier idle
    // snapshot, or in any later emission — re-queues the request behind a turn
    // or a parked interaction. The per-event fold stamps leftQueueAt and arms
    // the 15 s timer on EACH idle snapshot; without this symmetric fold a batch
    // shaped [idle snapshot, busy snapshot] left the timer armed, and it fired
    // an unconfirmed toast while the driver was still blocked. Withdraw the
    // stamp and cancel THAT nonce's timer; the next authoritative idle
    // re-stamps leftQueueAt (above) and re-arms the full window.
    let queueReentries: Map<Id, number> | null = null;
    if (merged.instances) {
      const pendingBase = merged.effortPending ?? this.state.effortPending;
      for (const next of merged.instances) {
        if (!effortAwaitsQueueExit(next.activity)) continue;
        const pending = pendingBase[next.id];
        if (pending && pending.leftQueueAt != null) {
          (queueReentries ??= new Map()).set(next.id, pending.nonce);
        }
      }
    }
    if (queueReentries?.size) {
      const base = merged.effortPending ?? this.state.effortPending;
      const pendingNext = { ...base };
      for (const [id, nonce] of queueReentries) {
        const pending = pendingNext[id];
        if (!pending || pending.nonce !== nonce || pending.leftQueueAt == null) {
          queueReentries.delete(id);
          continue;
        }
        pendingNext[id] = {
          ...pending,
          leftQueueAt: null,
          // The optimistic chip re-enters 排队中 for a client request; a wire
          // pending already carries the driver-provided queue word.
          ...(pending.source === "client" ? { queued: true } : {}),
        };
        const slot = this.effortReconcileTimers.get(id);
        if (slot?.nonce === nonce) {
          clearTimeout(slot.timer);
          this.effortReconcileTimers.delete(id);
        }
      }
      if (queueReentries.size) merged = { ...merged, effortPending: pendingNext };
    }
    this.state = { ...this.state, ...merged };
    for (const listener of this.listeners) listener();
    // Re-arm the request-owned bound for every request that just left its
    // queue (full window from the idle instant, same nonce ownership rules).
    if (queueExits?.size) {
      for (const [id, nonce] of queueExits) this.armEffortReconciliation(id, nonce);
    }
  }

  /**
   * Apply one newer effective projection to an in-flight request, settling the
   * axes the projection actually confirms (D-056: level and flag are separate
   * reads). Returns the updated pending entry, or null when BOTH axes have
   * settled and the indicator must be cleared.
   *
   * Request identity (c-effortread r10 items 1–4 / r19 item 1 ruling A) — a
   * pending request is settled ONLY by a requested echo carrying THIS
   * request's id (the driver echoed the configure nonce):
   *  1. an echo that carries ANOTHER id names a replaced request and is
   *     rejected outright — matching tier or a newer clock can never promote
   *     it;
   *  2. an ID-LESS projection settles NOTHING, however fresh or well matched.
   *     A missing id is not legacy-driver evidence: the current driver emits
   *     an id-less lifecycle whenever the configure omitted its optional
   *     requestId (another client), so an unknown session fails closed and
   *     waits for its own id evidence or its bounded unconfirmed path. A
   *     clamped or opposite outcome is therefore settled (and rendered as the
   *     delivered mismatch) only from the request's own id echo.
   */
  private settleEffortAxes(
    instanceId: Id,
    pending: EffortPending,
    view: EffortEffectiveView,
    requestedEcho?: { name?: string; ultracode?: boolean; requestId?: string },
  ): EffortPending | null {
    const echoId =
      typeof requestedEcho?.requestId === "string" && requestedEcho.requestId.trim()
        ? requestedEcho.requestId
        : null;
    // An echo that explicitly names ANOTHER request is never ours.
    const namesOtherRequest = echoId !== null && echoId !== pending.requestId;
    const echoNamesThisRequest = echoId !== null && echoId === pending.requestId;
    // c-effortread r11 item 2: an echo carrying a different request id is an
    // explicit veto — return the pending UNCHANGED, however many axes were
    // already settled (and so an untouched-but-settled axis can never combine
    // with it into the `levelSettled && flagSettled` clear).
    if (namesOtherRequest) return pending;

    const movesBothEcho = pending.commandWord === "ultracode";
    const movesLevel = movesBothEcho || pending.axis !== "flag";
    const movesFlag = movesBothEcho || pending.axis === "flag";

    // An echo naming THIS request is itself id-capability proof for the whole
    // session (e.g. the verdict echo arrives before the queued edge was
    // folded). Record it before any settlement return so even a request fully
    // settled by this echo upgrades the session and every later request fails
    // closed on id-less projections.
    const ownEcho = echoNamesThisRequest;
    if (ownEcho) this.noteEffortDriverId(instanceId, true);
    // c-effortread r19 item 1: with the legacy capability gone there is no
    // id-less echo matcher and no value fallback — only THIS request's own id
    // echo settles. Everything else leaves the pending untouched (the bounded
    // unconfirmed path ends an unknown request that never gets own-id proof).
    const ours = ownEcho;

    let { levelSettled, flagSettled } = pending;
    if (ours && !levelSettled && movesLevel) {
      // An echo that names this request settles the level even on a clamped
      // value (it is rendered as the delivered mismatch).
      levelSettled = true;
    }
    if (ours && !flagSettled && movesFlag) {
      const definitive = effortFlagSettles(view.ultracode);
      // A definitive flag value from our own verdict settles (an opposite
      // value is the delivered refusal).
      if (definitive) {
        flagSettled = true;
      }
    }
    if (ours && (levelSettled || flagSettled)) {
      this.settledEffortPushdown.set(instanceId, view.observedAt);
    }
    if (levelSettled && flagSettled) return null;
    const idCapability = ownEcho ? "id-capable" : pending.idCapability;
    if (idCapability === pending.idCapability) {
      return { ...pending, levelSettled, flagSettled };
    }
    return { ...pending, levelSettled, flagSettled, idCapability };
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
   * the new level. Each axis needs a projection strictly newer than the
   * request threshold, so a queued (working) switch and a replaced request's
   * stale projection are never cleared by the wrong read-back.
   */
  private hydrateEffortEffective(instances: Instance[]) {
    const next = { ...this.state.effortEffective };
    const withdrawnNext = { ...this.state.effortReadbackWithdrawn };
    const pendingNext = { ...this.state.effortPending };
    let effectiveUpdated = false;
    let pendingChanged = false;
    for (const instance of instances) {
      const id = instance.id;
      const view = effectiveFromRecord(instance.effortEffective);
      const previousPoll = this.polledEffort.get(id);
      // c-effortread r9 item 7: never let a rejected view advance polled
      // state. A null poll below sets it to null WITHOUT overwriting the
      // stored T1 projection, and a watermarked view below continues; storing
      // either first let a later null lower the watermark and let the rejected
      // view compare against itself.
      let pollAccepted = true;
      if (!view) {
        // r7 item 4(a): distinguish "the Hub projection went from a level to
        // null" (an explicit withdrawal) from "never projected". The pending
        // chip shows `?` only for the former.
        if (previousPoll) {
          // c-effortread r8 item 1 / r9 item 7: the watermark is the max of
          // the previous projection's observedAt and any live edge already
          // recorded (a readbackAvailable:false can land between polls) —
          // never lower it. The null record carries no timestamp.
          const liveWatermark = withdrawnNext[id];
          const stamp = (() => {
            const candidates = [previousPoll.observedAt || "", liveWatermark ?? ""].filter(Boolean);
            return candidates.reduce<string>(
              (newest, ts) =>
                newest && Date.parse(newest) > Date.parse(ts) ? newest : ts,
              "",
            );
          })();
          if (liveWatermark == null) withdrawnNext[id] = stamp || "";
          else withdrawnNext[id] = stamp || liveWatermark;
          delete next[id];
          effectiveUpdated = true;
        }
        this.polledEffort.set(id, view);
        continue;
      }
      // c-effortread r8 item 1: a poll carrying a view at or older than the
      // withdrawal watermark is stale (an in-flight poll with the
      // pre-withdrawal record) — keep the projection withdrawn, do not clear
      // the marker or move the slider.
      const watermark = withdrawnNext[id];
      if (watermark != null && (!view.observedAt || Date.parse(view.observedAt) <= Date.parse(watermark))) {
        pollAccepted = false;
        continue;
      }
      // A strictly newer real projection clears the withdrawal watermark.
      if (withdrawnNext[id] != null) {
        delete withdrawnNext[id];
        effectiveUpdated = true;
      }
      const current = next[id];
      // Compare PARSED instants: a same-second write with a different
      // fractional-second spelling must not look older or newer by string.
      if (!current || Date.parse(view.observedAt) >= Date.parse(current.observedAt)) {
        next[id] = view;
        effectiveUpdated = true;
        const pending = this.state.effortPending[id];
        if (pending) {
          const updated = this.settleEffortAxes(id, pending, view);
          if (updated) pendingNext[id] = updated;
          else delete pendingNext[id];
          pendingChanged = true;
        }
        // A positive switch read-back clears the refusal only for the model
        // the refusal was bound to (same scoping as the live path).
        if (view.ultracode === true && this.positiveFlagClearsEffortRefusal(id)) {
          this.clearEffortRefusal(id);
        }
      }
      // c-effortread r9 item 7: advance polled state only for an ACCEPTED
      // view, so a rejected watermarked view is never later compared against
      // itself and a following null still sees the pre-withdrawal projection.
      if (pollAccepted) this.polledEffort.set(id, view);
    }
    if (effectiveUpdated || pendingChanged) {
      this.emit({
        ...(effectiveUpdated
          ? { effortEffective: next, effortReadbackWithdrawn: withdrawnNext }
          : {}),
        ...(pendingChanged ? { effortPending: pendingNext } : {}),
      });
    }
  }

  /** Best-known current model for an instance: the transcript read-back, then
   *  the optimistic picker selection, then the launch spec's model. */
  private currentEffortModelId(instanceId: Id): string | null {
    const fromMap =
      this.state.modelEffective[instanceId]?.id ?? this.state.models[instanceId] ?? null;
    if (fromMap) return fromMap;
    const instance = this.state.instances.find((row) => row.id === instanceId);
    return instance?.model ? String(instance.model) : null;
  }

  /** Whether a positive ultracode read-back clears THIS refusal (c-effortui r3
   *  item 4): a process-scoped refusal always clears; a model-scoped one only
   *  on positive evidence for the model it was bound to. A null-bound refusal
   *  (older rows / direct seeds) is never auto-cleared. */
  private positiveFlagClearsEffortRefusal(instanceId: Id): boolean {
    const refusal = this.state.effortRefusal[instanceId];
    if (!refusal) return false;
    if (refusal.scope !== "model") return true;
    if (refusal.modelId == null) return false;
    return refusal.modelId === this.currentEffortModelId(instanceId);
  }

  private clearEffortRefusal(instanceId: Id) {
    if (!this.state.effortRefusal[instanceId]) return;
    const refusal = { ...this.state.effortRefusal };
    delete refusal[instanceId];
    this.emit({ effortRefusal: refusal });
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
   *  Settles the in-flight request per-axis and, when the change came from the
   *  terminal side, moves the slider/switch to the observed axes (single
   *  source of truth; never calls configure, so no push-down ping-pong).
   *
   *  `live` = false for replayed history (reconnect/catch-up): it HYDRATES
   *  effective state only and never settles/clears a pending, folds the
   *  optimistic selection, records or clears a refusal, or toasts (D-056 r2). */
  private noteEffortObservation(instanceId: Id, observation: Observation, live = true): boolean {
    const parsed = effectiveFromObservation(observation);
    if (!parsed) return false;
    // D-056 (4) read-back-unavailable edge: the driver withdrew the
    // projected level/flag (a verified resume became unverifiable). Clear the
    // live projection so the chip renders `?`, but do NOT settle or delete the
    // pending switch and do not move the optimistic slider — the configure
    // outcome (or its bounded timeout) owns the pending indicator.
    if (parsed.withdrawn) {
      const at = parsed.observedAt ?? null;
      const held = this.state.effortEffective[instanceId];
      // r6 item 8(a): a withdrawal arriving out of order (e.g. via
      // "Load earlier" AFTER a later valid edge M) is older history and must
      // not delete the newer projection.
      if (held && at && Date.parse(at) < Date.parse(held.observedAt)) {
        return false;
      }
      // c-effortread r8 item 1: raise/keep the watermark even when nothing is
      // currently held (reload with withdrawal in seed history, or an earlier
      // edge already cleared the projection), so a following OLDER level edge
      // (onPrepend replay, in-flight poll with the pre-withdrawal record) can
      // never re-project a withdrawn level.
      const watermarks = this.state.effortReadbackWithdrawn;
      const previous = watermarks[instanceId];
      if (at && previous && Date.parse(at) <= Date.parse(previous)) {
        // An older withdrawal changes nothing further; still clear a stale
        // projection if one exists.
        if (!held) return true;
      }
      const effortEffective = { ...this.state.effortEffective };
      delete effortEffective[instanceId];
      const nextWatermarks = { ...watermarks, [instanceId]: at ?? previous ?? "" };
      this.emit({ effortEffective, effortReadbackWithdrawn: nextWatermarks });
      return true;
    }
    const view = parsed.effective;
    if (!view) return false;
    const current = this.state.effortEffective[instanceId];
    if (current && Date.parse(view.observedAt) < Date.parse(current.observedAt)) return false;
    // c-effortread r8 item 1: reject any level view at or OLDER than the
    // withdrawal watermark — an older edge replayed via "Load earlier" (or an
    // in-flight poll carrying the pre-withdrawal record) must not restore the
    // withdrawn level. Only a strictly newer view clears the watermark.
    const withdrawnAt = this.state.effortReadbackWithdrawn[instanceId];
    if (withdrawnAt != null) {
      if (!view.observedAt || Date.parse(view.observedAt) <= Date.parse(withdrawnAt)) {
        return false;
      }
    }
    const withdrawnClear = withdrawnAt != null;
    const patch: Partial<HubState> = {
      effortEffective: {
        ...this.state.effortEffective,
        [instanceId]: view,
      },
      ...(withdrawnClear
        ? {
            effortReadbackWithdrawn: (() => {
              const next = { ...this.state.effortReadbackWithdrawn };
              delete next[instanceId];
              return next;
            })(),
          }
        : {}),
    };

    // Replayed history is read-only with respect to request-scoped state.
    if (!live) {
      this.emit(patch);
      return true;
    }

    // A positive switch read-back clears the refusal only on positive evidence
    // for the model it was bound to (a model-scoped refusal for another model
    // survives; a null-bound refusal can never be auto-cleared — r2 §5).
    if (view.ultracode === true && this.positiveFlagClearsEffortRefusal(instanceId)) {
      patch.effortRefusal = { ...this.state.effortRefusal };
      delete patch.effortRefusal[instanceId];
    }
    const pending = this.state.effortPending[instanceId];
    const hydratedAt = this.settledEffortPushdown.get(instanceId);
    // "Ours" = a live push-down still pending, OR one the durable projection
    // already settled whose live frame is only now arriving (same read-back,
    // observedAt no newer than the one the poll folded).
    const ours = Boolean(pending) || (hydratedAt != null && view.observedAt <= hydratedAt);
    if (ours) {
      if (pending) {
        const updated = this.settleEffortAxes(
          instanceId,
          pending,
          view,
          parsed.requested || parsed.requestId
            ? { ...parsed.requested, requestId: parsed.requestId }
            : undefined,
        );
        if (updated) {
          patch.effortPending = { ...this.state.effortPending, [instanceId]: updated };
        } else {
          patch.effortPending = { ...this.state.effortPending };
          delete patch.effortPending[instanceId];
        }
      }
      // Consume the marker once the matching (or an even newer) live edge for
      // our push-down arrives; a genuinely newer terminal switch (observedAt
      // past the marker) is handled in the else branch instead.
      if (hydratedAt != null && view.observedAt >= hydratedAt) {
        this.settledEffortPushdown.delete(instanceId);
      }
    } else {
      // Terminal-side switch (or an edge newer than any push-down we settled):
      // the observed axes are the truth — move the slider and the switch. This
      // is local state only, so it cannot re-trigger a configure.
      this.settledEffortPushdown.delete(instanceId);
      const instance = this.state.instances.find((row) => row.id === instanceId);
      const kind = (instance?.kind ?? "claude") as EffortKind;
      const selection = effortFromRecord(
        kind,
        view.name,
        null,
        view.ultracode === true,
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

  /** Fold one `instance.configure` effort lifecycle into the pending map.
   *  Replayed (`live=false`) lifecycles are ignored entirely: they never
   *  queue, apply, degrade, toast, touch pending, or record a refusal. */
  private noteEffortLifecycle(instanceId: Id, observation: Observation, live = true) {
    if (!live) return;
    const payload = observation.payload as
      | {
          type?: string;
          nativeName?: string;
          status?: unknown;
          relatedIds?: Record<string, unknown> | null;
        }
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
    // c-effortread r10: the driver echoes the configure's per-request id in
    // the lifecycle's related-ids map. An edge carrying an id settles only the
    // pending with the SAME id; an id-less edge falls back to the strict
    // command-word/value matcher (older drivers, the e2e fake node).
    const edgeRequestId =
      payload.relatedIds && typeof payload.relatedIds === "object"
        ? payload.relatedIds[EFFORT_REQUEST_ID_KEY]
        : undefined;
    const edgeId =
      typeof edgeRequestId === "string" && edgeRequestId.trim() ? edgeRequestId : null;
    const current = this.state.effortPending[instanceId];
    // c-effortread r18 item 2 / r19 item 1: only an id-bearing lifecycle
    // frame is per-SESSION capability evidence — it proves the driver echoes
    // per-request ids (id-capable, monotonic). An id-less frame proves NOTHING:
    // the current driver emits one whenever the configure omitted its OPTIONAL
    // requestId (e.g. ANOTHER CLIENT), so the capability (and a fresh session)
    // stays unknown and an unknown request keeps failing closed on its own id
    // evidence. The fact persists across requests.
    const frameCapability = this.noteEffortDriverId(instanceId, edgeId !== null);
    if (current && current.idCapability !== frameCapability) {
      // The pending snapshots the capability at arm time; keep the in-flight
      // entry consistent with what this frame proved (an id-bearing frame
      // upgrades an unknown pending to id-capable).
      this.emit({
        effortPending: {
          ...this.state.effortPending,
          [instanceId]: { ...current, idCapability: frameCapability },
        },
      });
    }
    // Whether a QUEUED edge belongs to the current pending: an id is
    // authoritative; an id-less edge matches only a pending SYNTHESIZED FROM
    // THE WIRE (a configure posted outside this UI) via the strict word/value
    // matcher. c-effortread r12 item 2: applied/degraded edges never use this
    // fallback — clearing and rollback require a non-empty matching id.
    const edgeBelongsTo = (pending: EffortPending, kind: EffortLifecycleKind): boolean => {
      if (edgeId !== null) return edgeId === pending.requestId;
      return (
        pending.source === "lifecycle" && effortLifecycleMatches(pending, parsed.word, kind)
      );
    };
    // THIS request's queued edge timestamp. Parsed as an instant like every
    // other observedAt comparison (r6 same-second fix).
    const queuedThreshold =
      typeof observation.observedAt === "string" && Number.isFinite(Date.parse(observation.observedAt))
        ? observation.observedAt
        : null;
    if (parsed.kind === "queued") {
      let current: EffortPending | undefined = this.state.effortPending[instanceId];
      if (current) {
        // A queued lifecycle belongs to the REQUEST in flight only. A late
        // `effort-queued` for request A after the user replaced it with B
        // carries a different id/axes and is dropped — it must never overwrite
        // B's pending indicator nor stamp B's queued time (A→B race).
        // c-effortread r19 item 1: an ID-LESS queued edge claims NOTHING and
        // stamps nothing on a client request — a missing id is not legacy
        // evidence (another client's configure omitted its requestId), and the
        // id-less value fallback it used to bound is gone. It is simply
        // dropped for a client-owned pending; a request settles only on its
        // own id evidence or its bounded unconfirmed path.
        if (!edgeBelongsTo(current, "queued")) {
          // c-effortread r15 item 3: a PROVABLY NEWER queued lifecycle (its
          // observedAt is strictly later in journal order than the pending's
          // own queued stamp) replaces a LIFECYCLE-owned pending — the driver
          // queue discards a replaced same-axis op, so the older request A
          // never executes and an observer tracking it would wait for a
          // verdict that never comes. The replacement gets the new edge's
          // id/target/timing. A delayed OLDER edge (or an unusable/missing
          // stamp on either side) is rejected: order unknown fails closed.
          // Client-owned pendings and their nonce/afterReplacement
          // protection never enter this path.
          // c-effortread r16 item 5: protocol timestamps are milliseconds, so
          // two queued lifecycles in the SAME millisecond tie on observedAt;
          // the journal SEQ is the total order and a strictly later seq still
          // proves the incoming edge newer, while an equal ms with an older or
          // missing seq is rejected.
          const incomingMs = queuedThreshold != null ? Date.parse(queuedThreshold) : Number.NaN;
          const establishedMs =
            current.queuedObservedAt != null ? Date.parse(current.queuedObservedAt) : Number.NaN;
          const replacesLifecycle =
            current.source === "lifecycle" &&
            Number.isFinite(incomingMs) &&
            Number.isFinite(establishedMs) &&
            current.queuedSeq != null &&
            lifecycleEdgeIsNewer(incomingMs, observation.seq, establishedMs, current.queuedSeq);
          if (!replacesLifecycle) return;
          // Fall through and synthesize the replacement pending from the
          // newer edge exactly like a fresh observer-side create below.
          current = undefined;
        }
      }
      if (!current) {
        // No client-side pending (the configure was posted outside the UI, or
        // a newer lifecycle just replaced a lifecycle-owned pending): surface
        // 排队中 from the lifecycle's own axes.
        const request = effortLifecycleRequest(parsed.word);
        const instance = this.state.instances.find((row) => row.id === instanceId);
        const kind = (instance?.kind ?? "claude") as EffortKind;
        const fallbackSelection =
          effortFromRecord(kind, request.name ?? null, null, request.ultracode === true)
          ?? effortAt(kind, DEFAULT_EFFORT_INDEX, request.ultracode === true);
        const nonce = (this.state.effortNonces[instanceId] ?? 0) + 1;
        const lowerWord = parsed.word.trim().toLowerCase();
        const wireAxis: EffortAxis =
          lifecycleNamesUltraWord(parsed.word) && lowerWord !== "ultracode" ? "flag" : "tier";
        const wireMovesBoth = lowerWord === "ultracode";
        const wireMovesLevel = wireMovesBoth || wireAxis !== "flag";
        const wireMovesFlag = wireMovesBoth || wireAxis === "flag";
        const existingView = this.state.effortEffective[instanceId];
        // c-effortread r13 item 3: the observer tab owns no client send. The
        // driver's `effort-queued` edge proves the request is queued (the chip
        // keeps 排队中), but the bounded reconciliation is anchored on CURRENT
        // activity evidence exactly like a client request: when the session is
        // already idle at receipt the bound starts now (a backlogged edge / an
        // accepted no-op); when a turn is running — or the session is parked
        // on waiting-interaction (r15 item 1) — it starts at the next
        // authoritative idle.
        const busyNow = effortAwaitsQueueExit(instance?.activity);
        const atCreate = Date.now();
        // The wire lifecycle carries capability only when it bears an id
        // (r18 item 2 / r19 item 1): an id-bearing queued edge proves
        // id-capable; an id-less one (another client's configure) leaves the
        // session unknown. This persists across requests (frameCapability was
        // recorded at lifecycle entry).
        const lifecycleCapability = frameCapability;
        current = {
          nonce,
          requestId: edgeId ?? `lifecycle:${nonce}`,
          // A decoupled toggle word moves the flag; a level word (and the
          // coupled bare `ultracode`, which is the xhigh level command) moves
          // the tier.
          axis: wireAxis,
          name: fallbackSelection.name,
          ultracode: fallbackSelection.ultracode === true,
          queued: true,
          at: atCreate,
          baselineObservedAt: existingView?.observedAt ?? null,
          queuedObservedAt: queuedThreshold,
          // r16 item 5: journal ordering provenance for equal-ms lifecycles.
          queuedSeq: String(observation.seq),
      // Axes this request does not move need no proof; every axis the
      // request MOVES starts UNSETTLED even when the current projection
      // already shows the requested value (c-effortread r11 item 2): a queued
      // B targeting the old observation used to start settled and a later
      // `levelSettled && flagSettled` cleared it on A's mismatching-id echo
      // or an unchanged baseline poll. An accepted no-op is settled later by
      // ITS OWN request-id echo.
      levelSettled: !wireMovesLevel,
      flagSettled: !wireMovesFlag,
          source: "lifecycle",
          // Synthesized from the driver's own queued word; the coupled bare
          // word moves BOTH axes (r15 item 2): keep the word itself so
          // own-id settlement knows the level and the flag are both in play.
          commandWord: wireMovesBoth ? lowerWord : null,
          afterReplacement: false,
          // Capability proven by THIS queued edge: id-bearing → id-capable;
          // an id-less wire lifecycle (another client) leaves it unknown.
          idCapability: lifecycleCapability,
          // Idle-at-receipt arms the bound immediately; a working session arms
          // it from the working→idle evidence.
          leftQueueAt: busyNow ? null : atCreate,
        };
        this.emit({ effortNonces: { ...this.state.effortNonces, [instanceId]: nonce } });
      }
      // Provenance (c-effortui r3 item 3 / r10 item 1): the queued stamp is
      // THIS request's own queued-lifecycle observedAt, kept SEPARATE from the
      // pre-request baseline. A duplicate queued edge keeps the same time; an
      // unparseable timestamp leaves a previous stamp untouched.
      //
      // c-effortread r14 item 1: the accepted bound is MONOTONIC for every
      // pending source. An observer/lifecycle pending bound by an edge
      // at T3 (B's own effort-queued:max) must not be REPLACED by a delayed
      // older same-word edge at T1 (the replaced request A's backlogged
      // queued edge): with T0 < T1 < T2 < T3, taking T1 let A's matching
      // read-back at T2 settle B. Keep the NEWEST VALID instant of the
      // existing stamp and the incoming edge. (Only an edge that names the
      // pending by id, or matches a lifecycle pending by word, reaches here;
      // an id-less edge for a client pending was dropped above.)
      const incomingQueuedMs =
        queuedThreshold != null ? Date.parse(queuedThreshold) : Number.NaN;
      const establishedQueuedMs =
        current.queuedObservedAt != null
          ? Date.parse(current.queuedObservedAt)
          : Number.NaN;
      const stamped =
        Number.isFinite(incomingQueuedMs) &&
        (!Number.isFinite(establishedQueuedMs) || incomingQueuedMs > establishedQueuedMs)
          ? queuedThreshold
          : current.queuedObservedAt;
      // r16 item 5: keep the queued SEQ monotonic in tandem with the queued
      // instant (newer ms, or the greater seq within one millisecond) so a
      // later duplicate/re-queue edge never lets a future replacement compare
      // against stale ordering provenance.
      const stampedSeq =
        Number.isFinite(incomingQueuedMs) &&
        (!Number.isFinite(establishedQueuedMs) ||
          lifecycleEdgeIsNewer(
            incomingQueuedMs,
            String(observation.seq),
            establishedQueuedMs,
            current.queuedSeq ?? "0",
          ))
          ? String(observation.seq)
          : current.queuedSeq;
      // A lifecycle-created entry carries the queue state derived from current
      // activity; a queued edge for an EXISTING request really did re-queue it:
      // if a turn is now running again — or the session parked on a
      // PermissionRequest/Elicitation (r15 item 1) — the bound waits for the
      // next authoritative idle evidence (r13 item 2/3).
      const createdFromWire = this.state.effortPending[instanceId]?.nonce !== current.nonce;
      const nextActivity = this.state.instances.find((row) => row.id === instanceId)?.activity;
      const requeuedBehindTurn =
        !createdFromWire &&
        effortAwaitsQueueExit(nextActivity) &&
        current.leftQueueAt != null;
      // The session map and an existing pending were updated with the frame's
      // capability at lifecycle entry; a freshly synthesized pending adopted
      // it directly. An id-less queued frame never downgrades an id-capable
      // session (noteEffortDriverId is monotonic).
      this.emit({
        effortPending: {
          ...this.state.effortPending,
          [instanceId]: createdFromWire
            ? current
            : {
                ...current,
                queued: true,
                queuedObservedAt: stamped,
                queuedSeq: stampedSeq,
                ...(requeuedBehindTurn ? { leftQueueAt: null } : {}),
              },
        },
      });
      if (createdFromWire) {
        // c-effortread r13 item 3 / r19 item 1: the observer tab's id-less
        // wire pending gets the same bounded, request-owned reconciliation as
        // a client request, owned by ITS nonce (a newer edge replaces;
        // settlement/cleanup of a stale nonce is a no-op). With the value
        // fallback gone an id-less wire request can never settle, so a silent
        // no-op or an unusable queued timestamp ends in the explicit
        // unconfirmed outcome instead of sitting on 切换中 for the 30-minute
        // max age.
        this.armEffortReconciliation(instanceId, current.nonce);
      }
      return;
    }
    if (parsed.kind === "applied") {
      if (!current) return;
      // c-effortread r12 item 2: clearing a pending from an `applied`
      // lifecycle requires a NON-EMPTY request id that names THIS pending,
      // even when the pending was synthesized from a wire lifecycle — an
      // observer tab's lifecycle pending is its view of another client's
      // queued request B, and an id-less applied with the same word can be a
      // replaced request A's stale verdict. An id-less success settles only
      // on the request's OWN id verdict/echo; with none, the pending ends
      // through its bounded unconfirmed path, never here.
      if (edgeId === null || edgeId !== current.requestId) return;
      // The driver reported acceptance: the projected read-back is the source
      // of truth, but clear the indicator now rather than leaving it on 切换中
      // when the observation is delayed. The slider stays where requested.
      const pendingNext = { ...this.state.effortPending };
      delete pendingNext[instanceId];
      this.settledEffortPushdown.delete(instanceId);
      this.emit({ effortPending: pendingNext });
      return;
    }
    // degraded
    const isUltraRefusal =
      parsed.reason === ULTRA_MODEL_REASON || parsed.reason === ULTRA_WORKFLOWS_REASON;
    // Request identity (c-effortui r2 item 2, r10 item 3, r12 item 2): does
    // this verdict name the CURRENT request?
    //  - no pending at all: an unsolicited verdict from a configure posted
    //    outside the UI — it applies (refusal recorded, tier untouched).
    //  - an edge carrying a request id: it mutates the pending ONLY when the
    //    id is the current one. Request A's timeout/cancellation can never
    //    delete or roll back newer request B.
    //  - id-less edge: it can NEVER clear or roll back a pending, not even a
    //    lifecycle-synthesized one (that pending is an observer tab's view of
    //    a queued request B, and the same word cannot distinguish B from a
    //    replaced A). An id-less success settles only on the request's own id
    //    evidence; otherwise the request stays pending until its bounded
    //    reconciliation (r12 item 5, r19 item 1).
    //  - genuine ultracode refusals (model/workflow) are environment evidence:
    //    they are recorded and disable the switch regardless, but mutate the
    //    pending ONLY when they are this request's verdict.
    const namesCurrent = current
      ? edgeId !== null && edgeId === current.requestId
      : false;
    const mutatesRequest = !current || namesCurrent;
    if (!mutatesRequest && !isUltraRefusal) return;

    const pendingNext = { ...this.state.effortPending };
    if (mutatesRequest) {
      delete pendingNext[instanceId];
      // The push-down ended via a lifecycle, not a projected read-back: no
      // settled edge owes a later live frame the "our push-down" treatment.
      this.settledEffortPushdown.delete(instanceId);
    }
    // The native side refused: revert the affected axes to the last observed
    // state (or drop the optimistic request so the record default returns).
    // An unsolicited ultracode refusal (no pending) leaves the tier untouched.
    const effective = this.state.effortEffective[instanceId];
    const effort = { ...this.state.effort };
    if (mutatesRequest && current) {
      if (effective) {
        const instance = this.state.instances.find((row) => row.id === instanceId);
        const kind = (instance?.kind ?? "claude") as EffortKind;
        const selection = effortFromRecord(kind, effective.name, null, effective.ultracode === true);
        if (selection) effort[instanceId] = selection;
      } else {
        delete effort[instanceId];
      }
    }
    // D-056 stable reason codes. The two ultracode refusals also DISABLE the
    // switch (model- or process-scoped); the others are plain rejections the
    // user can immediately retry.
    let toastReason: string = parsed.reason;
    const refusalPatch: Partial<HubState> = {};
    if (isUltraRefusal) {
      const scope = parsed.reason === ULTRA_MODEL_REASON ? "model" : "process";
      // Bind the refusal to the model it actually concerns even when the
      // verdict arrives before the model catalog/effective state: read-back
      // id, then the optimistic picker selection, then the launch spec model.
      // Only a session with no model identity at all is recorded null (c-effortui
      // r3 item 4 — a null refusal otherwise blocked every later model).
      const modelId = scope === "model" ? this.currentEffortModelId(instanceId) : null;
      refusalPatch.effortRefusal = {
        ...this.state.effortRefusal,
        [instanceId]: {
          reason: parsed.reason,
          scope,
          modelId,
          at: observation.observedAt ?? new Date().toISOString(),
        },
      };
      toastReason =
        scope === "model"
          ? `模型 ${modelId ?? "当前模型"} 不支持 ultracode`
          : "需要开启 dynamic workflows";
    } else if (!mutatesRequest) {
      // A stale level verdict for a replaced request: no visible mutation, no
      // toast — B is still in flight.
      return;
    } else {
      toastReason =
        ({ "dialog-kept": "已取消切换", "invalid-argument": "档位无效", "no-readback-within-window": "未收到回读" } as Record<string, string>)[
          parsed.reason
        ] ?? parsed.reason;
    }
    this.toast(`effort 切换被拒绝：${toastReason}`);
    this.emit({ effortPending: pendingNext, effort, ...refusalPatch });
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
  /**
   * Gate 43 (c): publish the SETTLED cached-session state after a successful
   * REST bootstrap whose durable storage is unavailable (or whose catch branch
   * keeps the cached session offline). ready is always true so AuthGate never
   * sticks on the loading screen; authed follows the cached device session.
   */
  private publishCachedSessionSettled(err: unknown): void {
    this.emit({
      ready: true,
      authed: true,
      session: readSession(),
      error: err ? (err instanceof Error ? err.message : String(err)) : null,
    });
  }

  /**
   * @returns installed when a current-epoch machine exists after this call;
   * obsolete when the caller's epoch/generation retired (never install for a
   * newer identity); unavailable when the CURRENT generation's storage failed
   * (the caller settles degraded without a machine — gate 43 item 2).
   */
  private async initConnection(offline: boolean, ext: number): Promise<BootstrapOutcome> {
    // Gate 39/40 R-b: the CALLER's external epoch (captured at its public
    // entry before its first await) is REQUIRED; this installer never reads a
    // fresh epoch for the caller.
    const epoch = ext;
    // Gate 44 item 1: capture the CREDENTIAL generation this installation is
    // for BEFORE awaiting the outbox. A subscriber-triggered login can acquire
    // the transition WHILE this call is parked in ensureOutbox: it bumps authGen
    // and disposes the outbox, so on resume storage may report "ok" while
    // this.outbox is null (the newer transition owns it). Treating that as
    // "unavailable" wrongly fulfilled this caller instead of the successor's
    // install. A generation mismatch is "obsolete" — checked immediately after
    // the await, BEFORE any storage-availability classification.
    const installGen = this.authGen;
    const sameIdentity = () => this.extEpoch === epoch && this.authGen === installGen;
    // c-reconnfu gate 34 item 4: a REPLACEMENT login keeps this.connection but
    // the old box was disposed by runAuthIdentityChange. Ensure a current-
    // generation outbox on EVERY init path even when the connection machine is
    // reused, otherwise pending durable rows sit unserviced until another send
    // or a reload. (When the connection is brand new this also runs, as before.)
    // Gate 39: an "obsolete" result (the caller's epoch moved while waiting on
    // an open transition) stops this install entirely — never adopt the box the
    // NEW identity created.
    await this.ensureOutbox(epoch);
    // Gate 44 item 1: revalidate BOTH identity coordinates immediately after
    // the storage await, before classifying availability (see installGen).
    if (!sameIdentity()) return "obsolete";
    // Gate 41 S1: a connection machine installed for THIS epoch is already
    // owned — either by a replacement login's reused machine or by a
    // concurrent same-epoch bootstrap that won the single-flight/reload race.
    // Adopt it and return: never restore bubbles twice or construct a second
    // machine (the second assignment would orphan the first, whose timers and
    // callbacks keep running).
    if (this.connection && this.connectionInstallEpoch === epoch) {
      return "installed";
    }
    // Gate 44 ruling U: storage UNAVAILABLE for the CURRENT generation still
    // installs the connection machine (and listeners) WITHOUT an outbox, so the
    // published connection state always comes from a live machine and the UI is
    // never stuck on the initial "recovering". Sends use the keyed direct
    // fallback; durable rows stay in storage and restore on the next reload —
    // there is deliberately NO in-page storage retry machinery here.
    const box = this.outbox;

    // Durable bubble restoration exists only when the box opened. Recheck
    // identity synchronously before the restore emit and machine creation.
    if (box && !sameIdentity()) return "obsolete";
    if (box) {
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
          if (r.state === "reconciling") this.reconcileReconcilingRow(r.instanceId, r.commandId, box);
        }
      }
    }

    // Gate 39/44 R-b: revalidate synchronously immediately before constructing
    // the connection machine; a retirement between the outbox install and here
    // stops the machine from being built or published.
    if (!sameIdentity()) {
      box?.dispose();
      return "obsolete";
    }
    const machine = new ConnectionMachine({
      resume: () => this.resumeConnection(),
      probe: () => this.connectionProbe(),
      isFollowLive: () => this.followSocketLive(),
      onState: (state) => {
        // Gate 41 S2: a callback from a machine that has since been disposed
        // and replaced (peer logout mid-probe, then a healthy re-login) must
        // never overwrite the installed machine's published state.
        if (this.connection !== machine) return;
        this.emit({ connection: state, outboxPending: box?.pendingCount() ?? 0 });
        // A socket-level recovery is also a moment to deliver the queue.
        if (state === "live") void this.flushAllOutbox();
      },
      onAttemptFinish: (info) => {
        if (this.connection !== machine) return;
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
    // Gate 41 S1: stamp the installing epoch so a concurrent same-epoch
    // initConnection adopts this machine instead of building another.
    this.connectionInstallEpoch = epoch;
    // Gate 39/44 R-b: do not start/attach a machine if the identity retired
    // during construction.
    if (!sameIdentity()) {
      machine.dispose();
      this.connection = null;
      this.connectionInstallEpoch = null;
      return "obsolete";
    }
    // A successful REST bootstrap proves reachability, but on the list/new
    // pages there is no follow socket to watchdog yet: start live WITHOUT a
    // frame timer. The instant a session's follow socket binds
    // (openFollowSocket → followBound) the framed-socket deadline takes over,
    // so an active session whose transcript is frozen can never show 已连接.
    // A failed bootstrap starts the offline reconnect loop instead.
    if (offline) machine.setStateOffline();
    else machine.bootstrapLive();
    // Ruling U: publish the machine's CURRENT state at install when the machine
    // did not emit it itself. It starts "offline" and setStateOffline is a
    // no-op when the state already matches, so without this the store kept its
    // INITIAL "recovering" value forever on an offline-with-unavailable-storage
    // reload. (bootstrapLive already emitted on the recovering/offline->live
    // change, so this is skipped in the healthy path.)
    if (this.state.connection !== machine.state) {
      this.emit({ connection: machine.state, outboxPending: box?.pendingCount() ?? 0 });
    }

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
    // No-op without an outbox (ruling U).
    if (box) void this.flushAllOutbox();
    // Ruling U: a machine installed with no outbox reports "unavailable" —
    // callers are fulfilled (machine + listeners own connection state), sends
    // use the keyed direct fallback and durable rows restore on next reload.
    return box ? "installed" : "unavailable";
  }

  private onConnOnline = () => this.connection?.dispatch({ type: "online" });
  private onConnOffline = () => this.connection?.dispatch({ type: "offline" });
  private onConnPageshow = (event: PageTransitionEvent) => {
    // BFCache restore (iOS app switch, Back): the earlier pagehide set the
    // unload flag, but the page is alive again — clear it so flushes resume.
    // The flag must never outlive the unload that set it.
    this.pageIsUnloading = false;
    this.unloadProceeded = false;
    this.outbox?.clearUnloading();
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
      this.outbox?.clearUnloading();
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
  setFollowLiveForTest(open: boolean, framed: boolean, journalId?: Id) {
    const j = journalId ?? this.connectionBoundJournal ?? "test_journal";
    if (open) this.followReadyState.set(j, () => 1);
    else this.followReadyState.delete(j);
    if (framed) this.followFrameAt.set(j, Date.now());
    else this.followFrameAt.delete(j);
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

  /**
   * Test-only: seed the per-session driver id-capability that a real
   * id-bearing lifecycle/echo would have established (c-effortread r18 item 2
   * / r19 item 1).
   */
  setEffortIdCapabilityForTest(instanceId: Id, capability: EffortDriverIdCapability): void {
    this.effortDriverId.set(instanceId, capability);
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
  /**
   * Test seams for the gate-33 dispose-on-auth-change invariant: expose the
   * current auth generation, the single identity-change helper, and a gate on
   * the outbox storage-open so a test can start initialization, change
   * identity before it completes, and assert the obsolete initializer installs
   * neither a box nor a flush.
   */
  get authGenForTest(): number {
    return this.authGen;
  }
  /** Test seam: current external-identity epoch (gate 40 item 3). */
  get extEpochForTest(): number {
    return this.extEpoch;
  }
  applyAuthIdentityChangeForTest(): void {
    this.applyAuthIdentityChange(true);
  }
  /** Test seam: true while a Fence-A credential transition is open. */
  get authTransitionOpenForTest(): boolean {
    return this.authTransition !== null;
  }
  /**
   * Test seam (gate 40 item 3): ensure the outbox for the CURRENT identity.
   * Production callers must call {@link ensureOutbox} with the epoch they
   * captured before their first await; tests act as the current identity.
   */
  ensureOutboxForTest(): Promise<"ok" | "obsolete" | "unavailable"> {
    return this.ensureOutbox(this.extEpoch);
  }

  private outboxOpenGateForTest: ((resolve: () => void) => void) | null = null;
  setOutboxOpenGateForTest(gate: ((resolve: () => void) => void) | null): void {
    this.outboxOpenGateForTest = gate;
  }
  /** Gate after the box is installed but before the initializer returns (gate 44). */
  private outboxInstalledGateForTest: ((resolve: () => void) => void) | null = null;
  setOutboxInstalledGateForTest(gate: ((resolve: () => void) => void) | null): void {
    this.outboxInstalledGateForTest = gate;
  }
  /**
   * Test seam (c-reconnfu gate 44 item 1): simulate a credential-generation
   * supersession WITHOUT an external-identity change — bumps authGen and
   * disposes the current outbox exactly like the credential part of an
   * interleaved login acquisition, leaving extEpoch current.
   */
  credentialGenerationSupersedeForTest(): void {
    this.authGen += 1;
    this.authBootstrapOwed = null;
    this.disposeOutboxForAuthChange();
  }
  /** Gate the Outbox.load step (second suspension point) for gate-34 item 5. */
  private outboxLoadGateForTest: ((resolve: () => void) => void) | null = null;
  /** Test seam: make outbox storage-open fail (gate-35 item 5 fallback). */
  private forceUnavailableStorageForTest = false;
  setForceUnavailableStorageForTest(v: boolean): void {
    this.forceUnavailableStorageForTest = v;
  }
  setOutboxLoadGateForTest(gate: ((resolve: () => void) => void) | null): void {
    this.outboxLoadGateForTest = gate;
  }
  /** Gate the durable enqueue storage.put for gate-34 item-1 public-send test. */
  private outboxEnqueueGateForTest: ((resolve: () => void) => void) | null = null;
  setOutboxEnqueueGateForTest(gate: ((resolve: () => void) => void) | null): void {
    this.outboxEnqueueGateForTest = gate;
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
      // Cancel EVERY central lease/delivery wake + bounded read retry (the
      // Outbox owns them post gate 20) so a tearing-down page keeps zero timers.
      this.outbox?.cancelAllWakesForUnload();
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
    this.outbox?.cancelAllWakesForUnload();
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
      this.outbox?.clearUnloading();
      void this.rearmLeaseWakeups();
    }, 0);
  };

  /**
   * Re-arm the central lease wakes from durable rows WITHOUT flushing: a
   * read-only authoritative refresh + reconciliation. Used by the
   * cancelled-beforeunload self-reset, which must not itself POST (the task
   * can run inside a real reload teardown). The reconciliation arms deadline
   * wakes for live leases and a 0 ms delivery wake for an overdue/stealable
   * row the prompt sat open past; that wake's fire → guarded flush is itself
   * blocked while pageIsUnloading stays set (a real pagehide cancels all
   * wakes via cancelAllWakesForUnload).
   *
   * c-reconnfu gate 21 item 4: a read that rejects ONCE here used to give up
   * permanently and lease recovery was lost (until an unrelated flush). The
   * read now goes through the BOUNDED, lifetime-aware recovery chain:
   * growing backoff, central reconciliation on success, a real pagehide
   * cancelling every timer, and an explicit exhausted surface at the limit.
   */
  private async rearmLeaseWakeups() {
    const box = this.outbox;
    if (!box || this.pageIsUnloading) return;
    const rows = await box.refreshDurableRecovering(0);
    // A real pagehide can land while the read/retry was suspended (the reset
    // task runs ~immediately before pagehide during a genuine reload): a
    // torn-down page applies no reconciliation, arms no timer — the bounded
    // chain itself was cancelled by cancelAllWakesForUnload.
    if (!rows || this.pageIsUnloading) return;
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

  /**
   * Adopt an ALREADY-MOUNTED follow's socket after bindConnection rebound the
   * connection machine to it (c-reconnfu gate 16 item 1). bindConnection's
   * noteBinding retires any in-flight resume and clears the watchdog, so the
   * machine would otherwise sit in `recovering` with NO timers (banner stuck,
   * create refused) because the mounted branch returns before
   * openFollowSocket. A live socket certifies live; a dead socket takes the
   * rebind path (re-arms stale timers or a bind deadline -> beginResume).
   */
  private adoptMountedFollow() {
    if (this.followSocketLive()) {
      this.connection?.followRebindLive();
    } else {
      this.connection?.followBound({ rebind: true });
    }
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
   *
   * `skipCurrentlyHeld` (gate 19 item 2) is the timer-fire policy: the held
   * suppression is enforced against the FRESH selection made INSIDE the
   * instance lock, not against the snapshot read before the flush waited on
   * that lock. Another tab can finish a row as "held" while this flush waits;
   * a snapshot id-set computed earlier misses it and used to re-POST the row
   * ahead of its held-retry deadline. The policy is checked again at the claim
   * boundary so the row under the lock is the row that is judged. Normal
   * flushes deliberately re-POST held rows (a host online→online resume
   * re-forwards them), so this is opt-in for the timer path only.
   */
  private async flushAllOutbox(attempted: Set<Id> = new Set(), skipCurrentlyHeld = false) {
    const box = this.outbox;
    if (!box) return;
    if (this.pageIsUnloading) return;
    // c-reconnfu gate 35 DELIVERY TOKEN: capture the box/generation/transition
    // identity ONCE for the whole flush; every lock turn and POST validates
    // against it instead of re-reading this.outbox. A stale token ends the
    // flush.
    const token = this.deliveryToken(box);
    const passTokens: number[] = [];
    try {
      // Drain passes; each pass acquires the lock, posts at most the FIRST
      // deliverable unattempted row, and releases — letting queued conversions
      // interleave.
      for (let pass = 0; pass < MAX_FLUSH_PASSES; pass += 1) {
        // c-reconnfu gate 22 item 6: this flush's tokens belong to the
        // outbox that issued them (`box`). A logout/reinit WHILE the flush
        // was queued (e.g. waiting on another instance's lock) replaces it;
        // the stale continuation must not read, lock or POST through the new
        // auth epoch. Stop here; the finally releases the tokens on the
        // ISSUING (now disposed) box and never reconciles the new one.
        // c-reconnfu gate 35: the delivery token must still be live before
        // EACH lock turn (not just at flush start); a transition that acquired
        // mid-flush ends the whole flush — the captured box never proceeds.
        if (!this.tokenLive(token)) return;
        if (this.outbox !== box) return;
        if (this.pageIsUnloading) return;
        // Pick the instance locks from FRESH durable rows, not the load-time
        // cache: another tab can persist a row after this tab loaded, and a
        // cache-only enumeration would never even attempt its instance lock.
        const now = Date.now();
        let durable;
        try {
          durable = await box.readDurable();
        } catch (err) {
          // flushAllOutbox is fire-and-forget from many call sites; a rejected
          // durable refresh must not become an unhandled rejection (gate 6 item
          // 4c). Surface it and stop this pass — gate 20 item 4: the failed
          // read KEEPS recovery responsibility via the bounded, lifetime-aware
          // retry that ends back in the central reconciliation. The finally
          // joins that SAME chain only AFTER releasing this flush's
          // suppression, so its reconciliation cannot be skipped as covered.
          this.reconcileToast(err, "刷新待发队列");
          void box.deliveryAttemptFailed();
          return;
        }
        const instances = [
          ...new Set(durable.filter((r) => isDeliverableOutbox(r, now)).map((r) => r.instanceId)),
        ];
        // This flush loop is the imminent guarded delivery for every instance
        // with a deliverable row: take a scoped token before applying the
        // CENTRAL reconciliation (c-reconnfu gate 20) so no parallel 0 ms
        // delivery wake is armed for rows THIS loop is about to claim. Live
        // leases get their one deadline wake; held rows their held-retry
        // clock.
        passTokens.push(box.beginDeliveryPass(instances));
        box.applyLeaseWakeReconcile(durable);
        let deliveredNew = false;
        for (const instanceId of instances) {
          if (this.pageIsUnloading || this.outbox !== box) return;
          try {
            const id = await box.withInstanceLock(instanceId, (iid, deliverable) =>
              this.deliverOneRow(
                token,
                iid,
                deliverable.filter(
                  (r) =>
                    !attempted.has(r.commandId) &&
                    // Gate 19 item 2: the policy rides into the FRESH selection
                    // built inside this lock turn — a row another tab finished
                    // as held while we waited is excluded here even though the
                    // pre-lock snapshot knew it as inflight.
                    !(skipCurrentlyHeld && r.state === "held"),
                ),
                { skipCurrentlyHeld },
              ),
            );
            // c-reconnfu gate 35 item 3: a stale lock turn ends the whole
            // flush immediately (no later command through the new box).
            if (id === "stale") return;
            if (id) {
              attempted.add(id);
              deliveredNew = true;
              // Progress (clearing the per-instance backoff/exhaustion) is
              // recorded at the shared POST boundary inside
              // deliverOutboxRecord's finally — the same boundary the direct
              // steer routes cross (c-reconnfu gate 23 item 3).
            }
          } catch {
            // The instance lock request itself rejected (deadlock/
            // SecurityError), or the no-Web-Locks lease wait ran past its
            // deadline. No double POST is possible, but the pass posts nothing
            // for this instance: count the bounded attempt so the fresh
            // reconciliation in the finally arms the backed-off delivery wake
            // instead of hot-looping, and keep draining other instances.
            box.noteDeliveryFailure(instanceId);
          }
        }
        if (!deliveredNew) break;
      }
    } finally {
      // Gate 22 item 6: release tokens (and reconcile) against the CAPTURED
      // outbox that issued this flush — `box`, never `this.outbox`, which a
      // logout/reinit may have replaced mid-queued-turn. Old tokens applied to
      // a new box would be meaningless suppression releases AND trigger a
      // stray reconciliation of the new auth epoch.
      if (this.pageIsUnloading) {
        // A genuine teardown owns all cancellation (cancelAllWakesForUnload
        // already cleared the books); just drop the tokens, arm nothing.
        for (const token of passTokens) box.releaseDeliveryPass(token);
        return;
      }
      if (this.outbox !== box) {
        // Issuing box was replaced: release its tokens on IT only. The
        // disposed old box ignores the epilogue; the new box is untouched.
        for (const token of passTokens) box.releaseDeliveryPass(token);
        return;
      }
      // Release ONLY this flush's suppression (overlapping flushes keep
      // theirs) and ALWAYS re-judge from a fresh authoritative snapshot: a
      // pass that posted nothing keeps exactly one wake per leftover row.
      void box.endDeliveryPasses(passTokens).catch(() => undefined);
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
  private async deliverOneRow(
    token: DeliveryToken,
    instanceId: Id,
    deliverable: OutboxRecord[],
    opts: { skipCurrentlyHeld?: boolean } = {},
  ): Promise<Id | null | "stale"> {
    const box = token.box;
    for (const rec of deliverable) {
      const fresh = box.get(rec.commandId) ?? rec;
      if (!isDeliverableOutbox(fresh, Date.now())) continue;
      // Gate 19 item 2: enforce the held policy a second time at the CLAIM
      // boundary. The candidate set was filtered from the fresh in-lock
      // selection, and the per-instance lock prevents another deliverer from
      // changing the row concurrently; this keeps the policy attached to the
      // exact row whose claim is about to be written.
      if (opts.skipCurrentlyHeld && fresh.state === "held") continue;
      const outcome = await this.deliverOutboxRecord(token, fresh);
      if (outcome === "stale") {
        // Gate 36 item 5: propagate the distinct STALE result to the lock
        // caller so a direct steer/held turn can return BEFORE touching
        // current-box state (this.outbox may be null mid-transition). The
        // flush caller ends the whole flush on it.
        return "stale";
      }
      if (outcome === "claim-aborted") {
        // FIFO: the first deliverable row could not be claimed this turn; a
        // later row in the SAME lock turn must never POST ahead of it — stop
        // this turn (the bounded recovery retries the first row), but this is
        // not a stale identity: other instances' turns may continue.
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
   *
   * `origin` is the outbox that handed the row here. A delayed ACCEPTANCE can
   * land seconds after the POST (c-reconnfu gate 25 item 1) — by then a
   * logout/reinit may have replaced the auth epoch; progress is recorded only
   * while `origin` is still the current outbox.
   */
  private reconcileReconcilingRow(instanceId: Id, commandId: Id, origin: Outbox): void {
    // One bounded GET loop per row per page lifetime: a row can be handed here
    // by both the POST landing and an outbox restore only across a real reload
    // (different page), but never run two loops concurrently in one page.
    if (this.reconcilingInFlight.has(commandId)) return;
    this.reconcilingInFlight.add(commandId);
    void this.runReconcileReconcilingRow(instanceId, commandId, origin).finally(() => {
      this.reconcilingInFlight.delete(commandId);
    });
  }

  private readonly reconcilingInFlight = new Set<Id>();

  private async runReconcileReconcilingRow(
    instanceId: Id,
    commandId: Id,
    origin: Outbox,
  ): Promise<void> {
    const deadline = Date.now() + RECONCILE_GET_DEADLINE_MS;
    for (;;) {
      // Journal evidence is authoritative acceptance (the Node ran the
      // command): the live/catch-up settleFromJournal already retired the row
      // to "done". Stop polling and never downgrade it with a GET verdict.
      // The done observation itself is an authoritative acceptance, so it
      // records progress through the same helper as every other path
      // (c-reconnfu gate 26).
      if (this.outbox?.get(commandId)?.state === "done") {
        await this.recordAcceptedDelivery(instanceId, commandId, origin);
        return;
      }
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
        // Rejection is not acceptance: the instance's exhaustion stays
        // latched and no delivery wakes are re-armed (gate 25 item 1).
        return;
      }
      if (verdict === "sent") {
        // The patch may be dropped as stale (a journal committed done while
        // the GET was in flight): acceptance is a property of the HUB'S
        // verdict, not of the local patch applying. Record progress regardless
        // (c-reconnfu gate 26).
        await this.reconcilePatch(commandId, { state: "sent", gotResponse: true });
        await this.recordAcceptedDelivery(instanceId, commandId, origin);
        return;
      }
      if (verdict === "held") {
        // Host went offline mid-reconcile: fall back to held retry (refund the
        // attempt and arm the bounded same-id re-POST). Held is not
        // acceptance: exhaustion stays latched until the held retry's own POST
        // is accepted at the shared boundary.
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
        // Never resolved: uncertainty to the end — exhaustion stays latched.
        return;
      }
      await new Promise((r) => setTimeout(r, RECONCILE_GET_INTERVAL_MS));
    }
  }

  /**
   * The SINGLE acceptance-progress boundary (c-reconnfu gate 26/27; gate 28
   * replaces the in-memory books with durable row state): progress is a
   * property of the HUB'S ACCEPTANCE of a command, not of which local path
   * observed it. Every authoritative acceptance observation routes through
   * here once — the POST sent verdict, the command-status GET (sent or a done
   * seen while polling), and a journal message settling the row
   * (live onEvents and catch-up settleFromJournal).
   *
   * The decision is entirely DURABLE: the row's `progressRecorded` flag flips
   * false → true in one compare-and-set that also requires
   * `issuerLifetime === this outbox's lifetime` (c-reconnfu gate 32: an auth
   * change always disposes the old outbox first, so lifetime is the whole
   * issuer identity). So:
   *  - progress for a command is recorded AT MOST ONCE, across the POST, GET
   *    and journal arriving in any order, regardless of in-memory cache size;
   *  - a cancelled navigation / BFCache suspension / reload cannot lose or
   *    duplicate it (it lives on the row, not in a per-outbox set);
   *  - an observation of a command issued by another outbox (a stale journal
   *    catch-up after logout/reinit/peer session removal, or another tab)
   *    never touches this outbox, though the journal done patch itself still
   *    retires the durable row;
   *  - rejected, failed, held or never-resolving outcomes never call this.
   */
  private async recordAcceptedDelivery(
    instanceId: Id,
    commandId: Id,
    origin?: Outbox | null,
  ): Promise<void> {
    const box = origin ?? this.outbox;
    if (this.pageIsUnloading || !box || this.outbox !== box) return;
    // c-reconnfu gate 32: the outbox page-lifetime is the sole issuer
    // identity. A live `box` is current by construction — logout/reinit (in
    // this page or observed from a peer) disposes it before this can run, and
    // every CAS retry re-checks that it is still the same live outbox.
    const lifetime = box.lifetime;
    let flipped = false;
    try {
      flipped = await box.casProgressRecorded(commandId, lifetime);
    } catch {
      // The Hub already accepted the command, but the durable CAS write
      // failed: the GET poll is over and the journal will not replay, so
      // swallowing here would lose the acceptance for good. Keep the
      // observation pending in the outbox's bounded, lifetime-aware recovery
      // (c-reconnfu gate 29 item 2): it re-tries until the CAS commits, the
      // row is gone/re-issued, the outbox is disposed, or the bounded cap.
      box.retryAcceptedProgress(instanceId, commandId, lifetime);
      return;
    }
    if (!flipped) return;
    box.noteDeliveryProgress(instanceId);
    await box.deliveryEpilogue().catch(() => undefined);
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
   * skip), `"claim-aborted"` when the first row still exists but could not be
   * claimed this turn, or `"stale"` (c-reconnfu gate 35) when the delivery
   * token's box/generation/transition is no longer current — the caller must
   * end the whole lock turn.
   */
  private async deliverOutboxRecord(
    token: DeliveryToken,
    current0: OutboxRecord,
  ): Promise<boolean | "claim-aborted" | "stale"> {
    const box = token.box;
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
    // A journal-confirmed terminal done checked FIRST (c-reconnfu gate 10
    // item 7): the CLAIM TRANSACTION may return a lease-less done (another tab
    // committed it between filtering and the merge). A lease-less done is a
    // definitive skip — never "claim-aborted", which ended the whole flush so
    // B never sent. Checked again after the claim below.
    if (current0.state === "done") return false;
    const isFreshAttempt = current0.state !== "held";
    // Gate 23 item 1 (claim identity ruling): EVERY claim attempt mints its
    // OWN unique token, persisted with the lease. If the claim's promise
    // rejects after commit, only a fresh durable read carrying THIS token can
    // establish the claim as won — the same owner's OLDER inflight lease (a
    // previous delivery whose finishing write aborted) is stale and must not
    // be POSTed under.
    const claimToken = localId();
    // c-reconnfu gate 32 single-writer invariant: the COMMITTED claim stamps
    // ONLY this outbox's page-lifetime id as the issuer. Auth identity changes
    // always dispose the old outbox (new lifetime) BEFORE anything runs, so the
    // lifetime alone separates every auth identity, tab and reload. Only this
    // write committing stamps it — a failed/never-attempted claim leaves it
    // unset.
    const claimLifetime = box.lifetime;
    let claimed;
    try {
      claimed = await box.patch(commandId, {
        state: "inflight",
        attempts: isFreshAttempt ? current0.attempts + 1 : current0.attempts,
        lease: { owner: box.ownerId, until: Date.now() + LEASE_TTL_MS, claimToken },
        issuerLifetime: claimLifetime,
      });
    } catch {
      // Gate 22 item 1 (coordinator ruling): a durable mutation that COMMITTED
      // is a success. The claim promise can reject AFTER mergeUnlessDone
      // committed this owner's inflight lease; one authoritative read decides.
      // Gate 23 item 1: the read is fresh (never the cache) and the row must
      // carry THIS attempt's token. A token match is a WON claim — POST under
      // it at once (no 30 s lease wait, attempts already holds the one claim
      // that reached the wire). An older lease of the same owner, or anything
      // else, keeps the claim-aborted path: FIFO stop plus the bounded,
      // lifetime-aware recovery.
      let won: OutboxRecord | null = null;
      try {
        const durablyCommitted = await box.committedIfAny(commandId, claimToken);
        if (
          durablyCommitted &&
          durablyCommitted.state === "inflight" &&
          durablyCommitted.lease?.owner === box.ownerId &&
          durablyCommitted.lease?.claimToken === claimToken
        ) {
          won = durablyCommitted;
        }
      } catch {
        won = null;
      }
      if (!won) {
        // Storage aborted the claim (and did not commit it): the first
        // deliverable row is still first and deliverable. Signal FIFO stop so
        // a later row is not POSTed ahead, and keep recovery responsibility:
        // the bounded, lifetime-aware retry ends back in the central
        // reconciliation which re-arms the row's one (backed-off) delivery
        // wake (c-reconnfu gate 20 item 4).
        void box.deliveryAttemptFailed([current0.instanceId]);
        return "claim-aborted";
      }
      claimed = won;
    }
    // A null claim means the row vanished inside the claim transaction
    // (another tab's retract deleted it — mergeUnlessDone resolves null
    // for a missing row): genuinely gone, safe to skip past.
    if (!claimed) return false;
    // Refuse a claim that did not come back with THIS tab's lease owner: a
    // foreign live lease is POSTing (or will POST) the row. Keep FIFO — do not
    // send a later row first this turn.
    // A journal-confirmed terminal done (possibly lease-less — another tab's
    // journal commit merged during the claim) wins over the lease check: skip
    // and continue the flush with B (gate 10 item 7).
    if (claimed.state === "done") return false;
    if (claimed.lease?.owner !== box.ownerId) return "claim-aborted";
    this.syncBubbleFromOutbox(commandId);
    // Gate 15 item 2: this document owns the live claim for the duration of the
    // POST — suppress expiry recovery for our own inflight row on refresh.
    box.beginActiveDelivery(commandId);

    const finish = async (patch: Partial<OutboxRecord>) => {
      await this.safePatch(commandId, { ...patch, lease: undefined });
    };
    // c-reconnfu gate 24 item 1: ONLY a POST the Hub actually ACCEPTED
    // (delivered/accepted verdict — durable state "sent") records progress and
    // clears the per-instance exhaustion. A rejected, failed (network/5xx/
    // 409), held, reconciling or otherwise uncertain outcome proves nothing
    // about delivery: the latch must survive it so the bounded wakes stay
    // cancelled. A held row has its own held-retry clock, which flushes
    // directly regardless of the latch. A journal "done" already returned at
    // the claim checks above, so the only acceptance here is the sent verdict.
    let accepted = false;
    let stale = false;

    try {
      // FENCE B / DELIVERY TOKEN (c-reconnfu gate 34 item 2, gate 35 item 3/4):
      // SYNCHRONOUS guard immediately before the POST, with no await between
      // the check and api.instanceSend. A stale token (box disposed/replaced,
      // generation or transition sequence moved, or a transition open) ends
      // the whole lock turn with a "stale" result.
      //
      // Item 4: the committed claim incremented attempts and put an inflight
      // lease on C. Restore the pre-claim retryable state and attempt count
      // and clear the lease — but ONLY while the stored claim token still
      // matches THIS aborted attempt (never undo a newer delivery's claim);
      // then leave recovery to the current box (the flush finalizer/next
      // trigger). C remains retryable at the SAME attempt number and is
      // delivered once, not flipped to "unknown" at the boundary.
      if (!this.tokenLive(token)) {
        await this.restoreAfterStaleClaim(token, commandId, claimToken, current0);
        stale = true;
        return "stale";
      }
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
        // A host online→online resume (flushAllOutbox) re-POSTs. Held is not
        // an accepted delivery: exhaustion is NOT cleared here.
        await finish({
          state: "held",
          serverState: result.command.state,
          gotResponse: true,
          attempts: current0.attempts,
        });
        this.scheduleHeldRetry(current0.instanceId);
      } else if (classified === "reconciling") {
        await finish({ state: "reconciling", serverState: result.command.state, gotResponse: true });
        await this.reconcileReconcilingRow(current0.instanceId, commandId, box);
      } else {
        await finish({ state: "sent", serverState: result.command.state, gotResponse: true });
        accepted = true;
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
          accepted = true;
        } else if (reconciled === "held") {
          await finish({ state: "held", gotResponse: true, attempts: current0.attempts });
          this.scheduleHeldRetry(current0.instanceId);
        } else if (reconciled === "reconciling") {
          await finish({ state: "reconciling", gotResponse: true });
          await this.reconcileReconcilingRow(current0.instanceId, commandId, box);
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
    } finally {
      box.endActiveDelivery(commandId);
      // c-reconnfu gate 23 item 3 / gate 24 item 1 / gate 26: the shared POST
      // boundary routes through the single acceptance helper — only a
      // Hub-ACCEPTED verdict clears the per-instance backoff/exhaustion and
      // re-runs the delivery epilogue; rejected/held/failed/uncertain/stale
      // outcomes leave the latch exactly as they found it.
      if (accepted) void this.recordAcceptedDelivery(current0.instanceId, commandId, box);
    }
    // A stale turn already restored the row and returned; skip the normal
    // post-POST bookkeeping (the current box owns recovery).
    if (stale) return "stale";
    this.syncBubbleFromOutbox(commandId);
    if (!box.pendingFor(current0.instanceId).length) this.clearOutboxRetry(current0.instanceId);
    return true;
  }

  /**
   * Restore the pre-claim retryable state after a STALE delivery token
   * (c-reconnfu gate 35 item 4). The committed claim wrote state=inflight,
   * attempts+1 and a live lease carrying `claimToken`. Undo those so the row
   * is retryable again at the SAME attempt number, but only while the stored
   * row still carries THIS attempt's claimToken — a newer delivery that won
   * the lease in between must never be reverted.
   */
  private async restoreAfterStaleClaim(
    token: DeliveryToken,
    commandId: Id,
    claimToken: Id,
    current0: OutboxRecord,
  ): Promise<void> {
    const priorAttempts = current0.attempts;
    const priorState: OutboxState = current0.state === "held" ? "held" : "pending";
    // The issuing (now disposed) box still owns its storage handle. The
    // conditional merge (claim-token guarded in the Outbox) reverts
    // inflight/attempts/lease ONLY if no newer delivery won the row.
    //
    // Gate 36 item 2 (ACCEPTED RISK, coordinator ruling): if THIS rollback
    // write itself fails, the obligation is deliberately NOT retried. The
    // narrow corner is storage failure + auth change at the 20th attempt: the
    // row can settle "unknown" at lease-expiry recovery instead of spending
    // its last POST. That is an accepted, VISIBLE state — no machinery is
    // added to chase an uncertain rollback commit.
    await token.box
      .restoreClaimIfTokenMatches(commandId, claimToken, priorState, priorAttempts)
      .catch(() => undefined);
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
      // c-reconnfu gate 26/27: a journal message settling a row as
      // accepted/done is an authoritative ACCEPTANCE observation routed
      // through the single helper — which records AT MOST ONCE per command and
      // ONLY for a command this outbox issued (a stale catch-up after
      // logout/reinit still retires the durable row below, but never clears the
      // new epoch's books). Non-message events and already-terminal
      // (done/rejected/unknown) rows record nothing.
      for (const ev of events) {
        if (ev.kind !== "message") continue;
        const commandId = (ev.payload as { commandId?: Id }).commandId;
        const rec = commandId ? this.outbox.get(commandId) : null;
        if (commandId && rec && rec.state !== "done" && rec.state !== "rejected" && rec.state !== "unknown") {
          // Journal evidence always retires the durable row, independent of
          // which epoch issued it; progress itself is gated inside the helper.
          void this.outbox.patch(commandId, { state: "done", serverState: "journal-settled" });
          void this.recordAcceptedDelivery(instanceId, commandId, this.outbox);
        }
      }
    }
    this.emit({ bubbles: next });
  }

  async bootstrap(): Promise<void> {
    // Gate 41 S1 / gate 42-43 — SINGLE-FLIGHT keyed on (extEpoch, authGen),
    // with TRI-STATE fulfilment (see BootstrapOutcome).
    this.registerPeerAuthWatch();
    await this.fulfilBootstrap(this.launchBootstrapRun(this.extEpoch, this.authGen), this.extEpoch);
  }

  /**
   * Launch a bootstrap run for an exact identity, coalescing an identical
   * in-flight one (StrictMode replay / duplicate reload). A newer identity
   * always starts a fresh run (it replaces the slot; the older run clears the
   * slot only when it still owns it).
   */
  private launchBootstrapRun(ext: number, gen: number): Promise<BootstrapOutcome> {
    const existing = this.bootstrapInFlight;
    if (existing && existing.ext === ext && existing.gen === gen) return existing.promise;
    const slot: { ext: number; gen: number; promise: Promise<BootstrapOutcome> } = {
      ext,
      gen,
      promise: undefined as unknown as Promise<BootstrapOutcome>,
    };
    const promise = this.runBootstrap(ext, gen);
    slot.promise = promise;
    this.bootstrapInFlight = slot;
    void promise.finally(() => {
      if (this.bootstrapInFlight === slot) this.bootstrapInFlight = null;
    });
    return promise;
  }

  /**
   * Fulfil one bootstrap caller from a run's tri-state outcome.
   *  - installed / unavailable: the UI is settled for this identity — done.
   *  - obsolete while the external epoch is STILL current: a same-epoch
   *    credential transition superseded this run. Follow the CURRENT
   *    generation's installation — join its in-flight run or start one — until
   *    it reports installed/unavailable (gate 43 item 1: the login's own
   *    callers, and a predecessor parked across a later FAILED successor, are
   *    fulfilled only once a current-gen machine or degraded state exists).
   *  - obsolete after the epoch moved: the identity logged out externally;
   *    settle silently (R-c already published the logged-out state).
   */
  private async fulfilBootstrap(first: Promise<BootstrapOutcome>, callerExt: number): Promise<void> {
    let result = await first;
    while (result === "obsolete" && this.extEpoch === callerExt) {
      const current = this.bootstrapInFlight;
      if (
        current &&
        current.promise !== first &&
        current.ext === this.extEpoch &&
        current.gen === this.authGen
      ) {
        result = await current.promise;
      } else {
        // No current-generation run (the successor failed without bootstrapping
        // or its run already drained): install for the current generation now.
        result = await this.launchBootstrapRun(this.extEpoch, this.authGen);
      }
    }
  }

  private async runBootstrap(extAtBoot: number, _genAtBoot: number): Promise<BootstrapOutcome> {

    // Gate 38 item 2: capture the external epoch at bootstrap start and
    // revalidate SYNCHRONOUSLY after every await below, before installing a
    // resource or publishing state. A peer logout / local logout / 401 while a
    // health or list request is in flight makes this bootstrap stop silently —
    // the external change already published logged-out and disposed the
    // outbox; this stale run must not recreate a connection/outbox or
    // republish authed from a cached session.
    const extLive = () => this.extEpoch === extAtBoot;
    // bootGen is still bumped so a list poll issued by an older bootstrap is
    // dropped when a newer one starts (refresh captures it), but it does NOT
    // retire bootstrap runs — single-flight + the external epoch own that
    // (gate 41 S1).
    ++this.bootGen;
    if (api.mock && readLoggedOut() && !readSession()) {
      this.emit({ ready: true, authed: false, session: null, devices: [], passkeys: [], connection: "offline" });
      return "installed";
    }
    if (api.mock && !readSession()) {
      const session = await api.login(MOCK_BOOTSTRAP_TOKEN, readDeviceSettings().deviceName);
      // Gate 40: epoch is the only retirement gate (not bootGen — see the catch
      // block); an external change while this step was parked retires the run.
      if (!extLive()) return "obsolete";
      // First identity establishment: route through the same helper so the
      // initial Outbox initializer starts under a defined, current generation.
      this.applyAuthIdentityChange();
      writeSession(session, { mock: api.mock });
      this.emit({ session: readSession() });
    }
    // Gate 43 item 1: if this run was launched by a SUPERSEDED predecessor's
    // settlement while the successor credential transition is still holding the
    // chain, wait for that transition instead of racing health/lists/install.
    // On success the successor's own settlement joins THIS single-flight run
    // (one install for both callers); if the successor fails without moving the
    // external epoch this run performs the current-generation recovery; if an
    // external change arrives meanwhile, retire silently (never hello).
    while (this.authTransition !== null && this.extEpoch === extAtBoot) {
      await this.authTransition;
    }
    if (!extLive()) return "obsolete";
    try {
      await api.hello();
      // Gate 40: epoch is the only retirement gate (not bootGen — see the catch
      // block); an external change while this step was parked retires the run.
      if (!extLive()) return "obsolete";
      if (!api.mock && !api.hasDeviceSession()) {
        this.emit({ ready: true, authed: false, error: null, connection: "live" });
        return "installed";
      }
      const [instances, hosts, interactions, devices, passkeys] = await Promise.all([
        api.instanceList(),
        api.hostList(),
        api.interactionList(),
        api.deviceList().catch(() => ({ items: [] as PairedDevice[] })),
        api.passkeyList().catch(() => ({ items: [] as PasskeyView[] })),
      ]);
      // Gate 40: epoch is the only retirement gate (not bootGen — see the catch
      // block); an external change while this step was parked retires the run.
      if (!extLive()) return "obsolete";
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
      // c-cardsettle: the Hub invalidates a session's pending card in the same
      // transaction that ends the instance. Its settlement notice makes every
      // mounted inbox/badge drop the card immediately via one trailing-coalesced
      // interaction refresh; missed notices converge on the next 2 s poll.
      this.stopSettlementFollow?.();
      this.stopSettlementFollow = api.settlementSubscribe((interactionId, reason) => {
        this.settlementCount += 1;
        // Install the terminal pin against the CURRENT list seq BEFORE
        // refreshing, so an older in-flight poll resolving last cannot
        // resurrect the settled card (r2 item 4).
        this.pinHubSettlement(interactionId, reason);
        if (this.settlementRefreshTimer) clearTimeout(this.settlementRefreshTimer);
        this.settlementRefreshTimer = setTimeout(() => {
          this.settlementRefreshTimer = null;
          void this.refresh().catch(() => undefined);
        }, 300);
      });
      // Re-validate immediately before installing the connection (and through
      // it the outbox): an external change landing during this synchronous
      // setup must not be followed by a stale machine/install, and again after
      // the install settles before starting polls.
      if (!extLive()) return "obsolete";
      // Gate 42/43: tri-state install. "installed" starts polls/dev handle.
      // "obsolete" is handed to the current generation by fulfilBootstrap (no
      // polls here). "unavailable" falls through to the settled degraded state.
      const install = await this.initConnection(false, extAtBoot);
      if (!extLive()) return "obsolete";
      if (install === "obsolete") return "obsolete";
      // installed OR unavailable (ruling U): the machine owns connection state.
      this.startPoll();
      this.installDevHandle();
      return install;
    } catch (err) {
      // Gate 41 item 1: RETIREMENT FIRST. If an external change (logout/401/
      // peer) advanced the epoch while the health/list request was parked —
      // possibly AFTER a replacement login had already completed its own
      // bootstrap — this stale run must not even classify the error: its late
      // 401 would otherwise applyAuthIdentityChange(true) + clearSession/drop
      // the cookie and destroy the replacement's session/outbox/connection. The
      // external change already published the logged-out state and tore the
      // old resources down. A CURRENT-epoch bootstrap's OWN 401 still passes
      // this check (the epoch has not moved yet) and performs the logout below.
      if (!extLive()) return "obsolete";
      const unauth = isUnauthorized(err);
      if (unauth) {
        // c-reconnfu gate 33 item 1: a 401 means the current session is gone;
        // dispose the outbox (and invalidate its initializer) before clearing
        // the session, so no delivery runs under the revoked identity. Gate 38:
        // this is an EXTERNAL change — bump extEpoch and drop the cached
        // session so any other suspended auth continuation stops silently.
        this.applyAuthIdentityChange(true);
        clearSession();
        dropDeviceCookie();
      }
      // A network failure WITH a device session still on disk is an offline
      // reload, not a logout: keep the (cached) authed UI, restore the outbox,
      // and let the connection machine reconnect. Only a real 401 logs out.
      const hasSession = api.hasDeviceSession();
      if (!unauth && hasSession) {
        const installOffline = await this.initConnection(true, extAtBoot);
        if (!extLive()) return "obsolete";
        // Gate 43 (b): gen-superseded while the epoch is current — never fulfil
        // here; fulfilBootstrap joins/starts the current generation's install.
        if (installOffline === "obsolete") return "obsolete";
        // installed OR unavailable (gate 43 (c)): either way settle the cached
        // authenticated session, start polling/recovery, and fulfil.
        this.publishCachedSessionSettled(err);
        this.startPoll();
        return installOffline; // "installed" | "unavailable"
      }
      this.emit({
        ready: true,
        authed: false,
        session: unauth ? null : this.state.session,
        error: err instanceof Error ? err.message : "bootstrap failed",
        connection: "offline",
      });
      return "installed";
    }
  }

  async login(kind: "bootstrap" | "pair", secret: string, deviceName: string) {
    // Gate 39 R-a: capture the EXTERNAL epoch at ENTRY, before the first await,
    // and carry that original value through the transition and settlement.
    const extAtEntry = this.extEpoch;
    // Fence A: dispose + block init BEFORE the credential request (the new
    // cookie lands in response headers); write the session only after the body
    // resolves. A body failure still releases the blocker and the chain's
    // final owner settles any pending bootstrap obligation (gate 36 item 4).
    const outcome = await this.runAuthTransition(
      () =>
        kind === "pair"
          ? api.pairRedeem(secret, deviceName)
          : api.login(secret, deviceName),
      extAtEntry,
    );
    // Gate 40 item 1: retired while QUEUED — the credential request was never
    // sent; settle silently (resolve, never throw).
    if (outcome.obsolete) return;
    if (!outcome.ok) {
      // A failed superseded caller transfers the obligation and settles on the
      // rejection WITHOUT awaiting its successor; the final owner owns it.
      await this.settleAuthBootstrap(outcome);
      throw outcome.error;
    }
    // Gate 38: an EXTERNAL logout/401/peer change while the credential body was
    // suspended retires this result — write/publish NOTHING (the promise still
    // resolves; the caller navigates nowhere because no session appeared).
    if (this.extEpoch !== outcome.acquired.ext) {
      await this.settleAuthBootstrap(outcome);
      return;
    }
    writeSession(outcome.result, { mock: api.mock });
    // bootstrap marks the session authenticated after cookie-backed reads finish.
    this.emit({ session: readSession(), error: null });
    // Gate 37 item 2: AWAIT the final-owner bootstrap so the login promise
    // resolves only after the authenticated UI is published — LoginPage
    // navigates in this promise's .then(), and resolving earlier made
    // AuthGate bounce back to /login (remount, passkey mediation restart).
    await this.settleAuthBootstrap(outcome);
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
    // Gate 39 R-a: capture the EXTERNAL epoch at ENTRY (before the challenge
    // request, the ceremony, and the finish transition) and revalidate after
    // every preliminary await. A peer/local logout while the challenge or the
    // WebAuthn assertion is parked must stop the flow BEFORE the finish
    // request is issued — no session, no bootstrap, and the promise settles
    // (resolves void), never hangs. The peer watch is registered here because
    // the credential transition (which otherwise registers it) has not started
    // when the challenge/ceremony await is parked.
    this.registerPeerAuthWatch();
    const extAtEntry = this.extEpoch;
    const retired = () => this.extEpoch !== extAtEntry;
    const envelope = await api.passkeyLoginStart(mediation === "conditional" ? "conditional" : undefined);
    if (retired()) return;
    const options = envelope.options as ServerRequestOptions;
    const assertion = await getPasskey(options, mediation, signal);
    if (retired()) return;
    const outcome = await this.runAuthTransition(
      () => api.passkeyLoginFinish(envelope.challengeId, assertion, deviceName),
      extAtEntry,
    );
    // Gate 40 item 1: retired while QUEUED — the finish request was never
    // sent; settle silently (resolve void, never throw).
    if (outcome.obsolete) return;
    if (!outcome.ok) {
      // A failed superseded caller transfers the obligation and settles on the
      // rejection WITHOUT awaiting its successor; the final owner owns it.
      await this.settleAuthBootstrap(outcome);
      throw outcome.error;
    }
    // Gate 38: suppress every effect of a result retired by an external change.
    if (this.extEpoch !== outcome.acquired.ext) {
      await this.settleAuthBootstrap(outcome);
      return;
    }
    writeSession(outcome.result, { mock: api.mock });
    this.emit({ session: readSession(), error: null });
    // Gate 37 item 2: await the final-owner bootstrap for the same reason as
    // password/pair login — no /login bounce after passkey navigation.
    await this.settleAuthBootstrap(outcome);
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

  /** Stop the periodic poll. Settlement-driven and user-triggered refreshes
   * still run; used by the cardsettle hub spec to prove a card drops from the
   * settlement frame rather than a poll. */
  stopPoll() {
    if (this.pollTimer != null && typeof window !== "undefined") {
      window.clearInterval(this.pollTimer);
    }
    this.pollTimer = null;
  }

  /** Dev-only debug handle (the hub e2e runs against the vite dev server):
   * lets a spec halt the 2 s interaction poll so a UI change can be
   * attributed to the settlement follow frame instead of polling. */
  private installDevHandle() {
    if (!import.meta.env.DEV || typeof window === "undefined") return;
    (window as unknown as {
      __remudaHub?: {
        stopPoll: () => void;
        settlementCount: () => number;
        interactionState: (id: string) => string | undefined;
        waitFollowCaughtUp: (id: string, timeoutMs?: number) => Promise<string>;
      };
    }).__remudaHub = {
      stopPoll: () => this.stopPoll(),
      settlementCount: () => this.settlementCount,
      interactionState: (id: string) =>
        this.state.interactions.find((row) => row.id === id)?.state,
      waitFollowCaughtUp: (id: string, timeoutMs?: number) =>
        this.waitForFollowCaughtUpForTest(id, timeoutMs),
    };
  }

  private clearDevHandle() {
    if (typeof window !== "undefined") {
      delete (window as unknown as { __remudaHub?: unknown }).__remudaHub;
    }
  }

  logout() {
    const mine = this.state.session?.deviceId;
    if (mine) void api.deviceRevoke(mine).catch(() => undefined);
    clearSession();
    dropDeviceCookie();
    api.disconnect();
    this.stopPoll();
    this.clearDevHandle();
    this.stopWorkspaceFollow?.();
    this.stopWorkspaceFollow = null;
    this.stopSettlementFollow?.();
    this.stopSettlementFollow = null;
    if (this.settlementRefreshTimer) {
      clearTimeout(this.settlementRefreshTimer);
      this.settlementRefreshTimer = null;
    }
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
    this.polledEffort.clear();
    // Tear down the connection machine and its follow bookkeeping (init
    // recreates them at the next bootstrap); the durable outbox itself stays.
    this.disposeConnectionForAuthChange();
    for (const [, t] of this.outboxRetryTimer) clearTimeout(t);
    this.outboxRetryTimer.clear();
    this.outboxRetryAttempt.clear();
    for (const [, t] of this.heldRetryTimer) clearTimeout(t);
    this.heldRetryTimer.clear();
    this.heldRetryAttempt.clear();
    for (const [, slot] of this.effortReconcileTimers) clearTimeout(slot.timer);
    this.effortReconcileTimers.clear();
    // c-reconnfu gate 33 item 1: logout goes through the single
    // identity-change helper — bumps authGen and disposes the outbox (and any
    // in-flight initializer) BEFORE the logged-out identity can be used; the
    // next bootstrap loads a fresh Outbox with a new lifetime. The helper also
    // nulls the pending bootstrap obligation (gate 37 item 1). Gate 38:
    // EXTERNAL — bump extEpoch and clear the cached session so suspended login
    // continuations and an in-flight bootstrap publish nothing afterwards.
    this.applyAuthIdentityChange(true);
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
    // Own the connection machine through bindConnection BEFORE any other
    // binding work (c-reconnfu gate 15 item 3): it registers this journal as
    // the bound session, bumps the binding generation on a real bind-away, and
    // runs the abandoned-session REST fallback when the instance we left had a
    // deferred catch-up. Assigning the fields directly (as the old code did)
    // stranded session A's accepted-but-unjoined row when the user navigated
    // to an already-mounted B — the mounted-journal early return never reached
    // openFollowSocket, so bindConnection's fallback never ran.
    this.bindConnection(instance.id, instance.journalId);
    const bindGen = this.connectionBindGen;
    if (!this.state.instances.some((i) => i.id === instanceId)) {
      this.emit({ instances: [instance, ...this.state.instances] });
    }
    // Fold projected effective state/catalog the single record carries.
    this.hydrateEffortEffective([instance]);
    this.hydrateModels([instance]);
    // Already mounted (SessionPage re-render): the bind above rebound the
    // connection machine to this active follow (and serviced any abandoned
    // instance); adopt THIS journal's socket so the rebound machine is not
    // left timer-less in recovering (gate 16 item 1).
    if (this.journals.has(instance.journalId)) {
      this.adoptMountedFollow();
      return;
    }
    this.emit({ journalStatus: { ...this.state.journalStatus, [instanceId]: "live" } });
    // Gate 17 item 1: an UNMOUNTED follow owns the machine while its seed and
    // first subscribe are in flight. bindConnection's noteBinding retired the
    // session-A attempt (and its watchdog) when we navigated here; without an
    // attempt of our own, a seed that rejects/times out left the machine in
    // recovering with NO timer (banner stuck, interrupt disabled, creates
    // refused) because openFollowSocket — the only followBound caller — was
    // never reached. Bracketing with the captured binding generation (the
    // mount settles only while it is still current) makes a failed seed take
    // the normal failure path: probe -> stale/offline -> retry, which reopens
    // through reopenFollow -> follow(). Nested inside a resume for the same
    // binding the begin shares the enclosing attempt and its watchdog.
    const mountAttemptId = this.connection ? this.connection.followAttemptBegin(bindGen) : null;
    try {
      // Seed from ONE bounded tail window (newest rows win). The old ascending
      // 512-loop sliced the front of the tail, silently dropping everything
      // below the middle of the window on a long journal. Older rows load on
      // demand via JournalClient.loadEarlier.
      const seed = await api.eventsRead({ journalId: instance.journalId, limit: 2000 });
      // Another mount may finish loading this journal while this read is pending.
      if (this.journals.has(instance.journalId)) {
        // Same adoption as the top mounted branch (gate 16 item 1), SCOPED to
        // this mount's captured generation+attempt (gate 17 item 1): a deferred
        // duplicate seed from an obsolete navigation (rapid A -> B -> A -> B)
        // must not certify the newest mount's machine. The winning mount's own
        // bracket settles the attempt when its socket is not live yet.
        if (this.connection && mountAttemptId !== null && this.followSocketLive()) {
          this.connection.followAttemptLive(mountAttemptId, bindGen);
          return;
        }
        // Gate 18 item 5: the journal can be mounted WITHOUT a live socket:
        // an earlier attempt for this same navigation won the journals.set
        // race but its openFollowSocket then rejected. The current call's
        // attempt is still in flight (the obsolete attempt's failure was
        // dropped on the generation guard), and the bare return left it in
        // recovering with nothing but the 20 s watchdog. End it as failed
        // while its binding is still current so the ordinary probe ->
        // stale/offline -> reopenFollow -> follow() recovery owns retry; a
        // superseded call is dropped inside followAttemptEnd as everywhere
        // else.
        if (
          this.connection &&
          mountAttemptId !== null &&
          bindGen === this.connectionBindGen &&
          !this.followSocketLive()
        ) {
          this.connection.followAttemptEnd(false, mountAttemptId, bindGen);
        }
        return;
      }
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
      // live=false: hydrate effective only, never settle pending/refusal.
    for (const event of history) {
        this.noteEffortObservation(instanceId, event, false);
        this.noteEffortLifecycle(instanceId, event, false);
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
            // c-effortread r16 item 4: fold THIS event's activity evidence
            // BEFORE its lifecycle reducers, in journal (seq) order, so a
            // queued edge arriving in the same batch as a snapshot parked on
            // waiting-interaction initialises the execution deadline from the
            // batch's own activity in order.
            const nextInstances = applyInstanceActivity(this.state.instances, [event]);
            const activityChanged = nextInstances.some(
              (row, index) => row !== this.state.instances[index],
            );
            if (activityChanged) this.emit({ instances: nextInstances });
            this.noteEffortObservation(instanceId, event, true);
            this.noteEffortLifecycle(instanceId, event, true);
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
            // c-reconnfu gate 26/27: each journal-done retirement is an
            // authoritative acceptance observation routed through the single
            // progress helper, which records at most once and only for a
            // command THIS outbox issued (a stale batch after reinit still
            // retires the row below but cannot clear the new epoch's books).
            // A row already rejected/terminal records nothing: a stray/late
            // journal event after a Hub rejection must not clear exhaustion.
            for (const ev of next) {
              if (ev.kind !== "message") continue;
              const commandId = (ev.payload as { commandId?: Id }).commandId;
              const rec = commandId ? this.outbox.get(commandId) : null;
              if (commandId && rec && rec.state !== "done" && rec.state !== "rejected") {
                void this.outbox.patch(commandId, { state: "done", serverState: "journal-settled" });
                void this.recordAcceptedDelivery(instanceId, commandId, this.outbox);
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
          // Prepended rows are HISTORY, not live frames: hydrate effective only.
          for (const event of fresh) {
            this.noteEffortObservation(instanceId, event, false);
            this.noteEffortLifecycle(instanceId, event, false);
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
      // The seed AND the first subscribe succeeded for the CURRENT binding:
      // certify the mount's attempt (a frame already may have). Scoped by the
      // generation captured at bind time — a navigation that landed mid-mount
      // makes this end a no-op.
      if (this.connection && mountAttemptId !== null) {
        this.connection.followAttemptEnd(true, mountAttemptId, bindGen);
      }
      // (connectionBoundTo/Journal were bound at the top of follow(), so the
      // machine owns this session even when this first open failed offline.)
    } catch (err) {
      // Seed read or first subscribe failed: hand the failure to the machine
      // for the captured binding so the NORMAL retry path (probe -> stale/
      // offline -> reconnect reopen) owns recovery instead of leaving a
      // timer-less recovering. Rethrow: callers (and the resume chain awaiting
      // follow()) must observe the failed mount as before.
      if (this.connection && mountAttemptId !== null) {
        this.connection.followAttemptEnd(false, mountAttemptId, bindGen);
      }
      throw err;
    }
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
      // c-effortread r9 e2e seam: the server subscribes to the live bus BEFORE
      // reading this snapshot (closing the seed/subscribe window), so once the
      // snapshot is applied over an open socket, every event PUBLISHED
      // AFTERWARDS is delivered as a genuine live frame and can never be
      // absorbed into the seed as replay. Resolve the caught-up waiter here.
      if (sub.getReadyState() === 1) {
        this.resolveFollowCaughtUp(instance.id, sub.snapshot.asOfSeq);
      }
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
    // c-effortui r3 item 9: seed BOTH axes. The D-056 create wire carries the
    // flag as its own boolean; ignoring it left a fresh Start showing the
    // switch off even though the launch argv was --effort ultracode (the next
    // slider drag would then post ultracode:false). "ultracode" no longer
    // exists as a tier name — the migration lives inside effortFromRecord.
    const effort =
      spec.effortName != null
        ? effortFromRecord(kind, spec.effortName, spec.effortIndex, spec.effortUltracode === true) ??
          effortAt(kind, spec.effortIndex ?? DEFAULT_EFFORT_INDEX, spec.effortUltracode === true)
        : effortAt(kind, spec.effortIndex ?? DEFAULT_EFFORT_INDEX, spec.effortUltracode === true);
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

    // c-reconnfu gate 35 item 5: capture the identity (gen + transition seq)
    // BEFORE the async availability check. A fallback POST validates this
    // synchronously at the wire; if a credential transition landed while
    // ensureOutbox was awaiting, the fallback must not POST (it has no box, so
    // the token carries gen/seq only — tokenLiveFallback checks those).
    const sendGen = this.authGen;
    const sendTransitionSeq = this.transitionSeq;
    const sendTransitionOpen = this.authTransition !== null;
    // Gate 39 R-b: this public entry's external epoch, threaded into
    // ensureOutbox — the outbox installer must never adopt a newer epoch on
    // this send's behalf.
    const sendExt = this.extEpoch;

    const outboxStatus = await this.ensureOutbox(sendExt);
    // c-reconnfu gate 34 item 1:
    //  - unavailable storage: still mint C so the legacy direct POST carries
    //    it as the dedupe id (never an unkeyed POST).
    //  - obsolete initializer (auth changed while opening): do NOT mint/post
    //    here; re-check once for the CURRENT generation's box. If it is now
    //    available the send proceeds normally under it; otherwise stop without
    //    any POST (no durable row was created, no action executed).
    if (outboxStatus === "obsolete") {
      const retry = await this.ensureOutbox(sendExt);
      if (retry !== "ok") return false;
    }
    const haveOutbox = this.outbox !== null;
    // A command id is minted whenever we MIGHT POST: durable enqueue, or the
    // legacy direct fallback when storage is genuinely unavailable.
    const commandId: string | null = haveOutbox || outboxStatus === "unavailable" ? newCommandId() : null;
    const createdAtMs = Date.now();
    const journalIdForRow = this.state.instances.find((i) => i.id === instanceId)?.journalId;
    // Persist the upload manifest refs (objectId/index/kind/…), never the
    // blob-only preview urls: the outbox survives reloads and POSTs the
    // canonical refs. The rendered bubble keeps the local preview urls.
    const persistableAttachments = attachments.filter(
      (a): a is AttachmentRef => Boolean(a.objectId),
    );
    // c-reconnfu gate 34 item 1: set true once C's enqueue commits. A send
    // that committed C never POSTs directly after an auth disposal.
    let committedDurably = false;
    if (this.outbox && commandId) {
      const outbox = this.outbox;
      // Storage-first: the bubble renders only AFTER the durable commit.
      // Gate 22 item 1 ruling: a committed enqueue always proceeds; a
      // transaction that really aborted retries the SAME commandId once.
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
      // Gate 22 item 1 (coordinator ruling): the durable commit is the
      // success. enqueueOnce only reports failure when the row did NOT commit
      // — a promise that rejects after storage.put committed is resolved by
      // one authoritative read, so the committed intent proceeds (one bubble,
      // one command id) instead of being retried into a second write or
      // reported as an honest failure. A genuine commit failure retries the
      // SAME commandId once; the second rejection is confirmed against
      // durable storage before giving up (never a phantom bubble or a second
      // id).
      const enqueueOnce = async (): Promise<boolean> => {
        try {
          if (this.outboxEnqueueGateForTest) {
            await new Promise<void>((resolve) => this.outboxEnqueueGateForTest?.(resolve));
          }
          await outbox.enqueue(enqueueRecord);
          committedDurably = true;
          return true;
        } catch {
          const seen = await outbox.committedIfAny(commandId).catch(() => null);
          if (seen) committedDurably = true;
          return seen !== null;
        }
      };
      if (!(await enqueueOnce()) && !(await enqueueOnce())) return false;
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
      // c-reconnfu gate 34 item 1:
      //  - C committed durably but the box was disposed (auth change) AFTER
      //    the commit: NEVER POST directly — C is durable and the
      //    current-generation box's recovery delivers it exactly once.
      //  - storage was genuinely unavailable (no commit): the legacy direct
      //    POST runs, ALWAYS carrying C as the Hub/Node dedupe id.
      if (committedDurably) return false;
      if (!commandId) return false;
      // c-reconnfu gate 35 item 5: gate the fallback POST synchronously with
      // the identity captured before the availability await. A credential
      // transition in the interim stops the send without posting.
      if (!this.fallbackTokenLive(sendGen, sendTransitionSeq, sendTransitionOpen)) return false;
      return this.sendWithoutOutbox(
        instanceId,
        prompt,
        attachments,
        effectiveMode,
        clientRequestId,
        commandId,
        () => this.fallbackTokenLive(sendGen, sendTransitionSeq, sendTransitionOpen),
      );
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
      //
      // c-reconnfu gate 22 item 2: the direct route ends through the SAME
      // shared epilogue as the guarded flush. The delivery turn clears the
      // active marker in its own finally; if the finishing write then ABORTS
      // (verdict in hand, durable row still inflight under this owner's live
      // lease) no patch epilogue ran, and reconciliations by OTHER instances'
      // flushes armed nothing while the POST was active — without the fresh
      // authoritative reconcile here the row would strand with no deadline
      // wake. A rejected lock/lease turn is attributed like a flush failure so
      // the bounded backoff applies.
      let steerFailed = false;
      let steerStale = false;
      const steerToken = this.deliveryToken(this.outbox);
      try {
        const lockResult = await steerToken.box.withInstanceLock(instanceId, (iid, rows) =>
          this.deliverOneRow(steerToken, iid, rows),
        );
        if (lockResult === "stale") steerStale = true;
      } catch {
        // Gate 36 item 5: attribute against the CAPTURED box — the current
        // outbox may already be null (an auth transition disposed it).
        steerToken.box.noteDeliveryFailure(instanceId);
        steerFailed = true;
      }
      // Gate 36 item 5: the turn's identity was replaced while the claim was
      // parked (this.outbox may be null). Return BEFORE reading any
      // current-box state; the replacement identity's finalizer owns
      // delivery of the durable row.
      if (steerStale) return false;
      await steerToken.box.deliveryEpilogue().catch(() => undefined);
      if (steerFailed) return false;
      this.syncBubbleFromOutbox(commandId);
      const finalState = steerToken.box.get(commandId)?.state;
      return finalState === "sent" || finalState === "done";
    }
    // Online ordinary send: flush now in the background; the POST never
    // blocks the caller.
    void this.flushAllOutbox();
    return true;
  }

  /**
   * Legacy direct-POST path used only when durable storage is unavailable.
   * c-reconnfu gate 34 item 1: it ALWAYS receives the client command id and
   * passes it to the POST as the Hub/Node dedupe id — an unkeyed fallback POST
   * could not be deduplicated against any later durable replay.
   */
  private async sendWithoutOutbox(
    instanceId: Id,
    prompt: string,
    attachments: AttachmentRef[],
    mode: PromptMode | undefined,
    clientRequestId: Id,
    commandId: Id,
    tokenLiveNow: () => boolean = () => true,
  ): Promise<boolean> {
    try {
      // c-reconnfu gate 35 item 5: synchronous, adjacent identity check right
      // before the fallback POST (no await between check and instanceSend).
      if (!tokenLiveNow()) return false;
      const result = await api.instanceSend(instanceId, prompt, attachments, mode, commandId);
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
    // Gate 39 R-b: pass this entry's epoch to the installer (captured
    // synchronously — no await precedes it here).
    if ((await this.ensureOutbox(this.extEpoch)) !== "ok") return;
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
    // Gate 39 R-b: this entry's external epoch (synchronous capture — the
    // enqueue below is the first await) goes to the installer.
    const steerExt = this.extEpoch;
    if ((await this.ensureOutbox(steerExt)) !== "ok" || !this.outbox) return false;

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
    // Gate 22 item 2: this direct entry ends through the SAME shared
    // epilogue as the live steer route and the guarded flush — a finishing
    // write that aborts leaves the row inflight with no patch epilogue.
    let steerFailed = false;
    let steerStale = false;
    // Gate 36 item 5: the enqueue await above is a suspension point — an auth
    // transition may have disposed the box since ensureOutbox. With no live
    // box the replacement identity owns delivery; report the steer as not yet
    // accepted rather than dereferencing null.
    const heldBox = this.outbox;
    if (!heldBox) return false;
    const heldSteerToken = this.deliveryToken(heldBox);
    try {
      const lockResult = await heldSteerToken.box.withInstanceLock(instanceId, (id, rows) =>
        this.deliverOneRow(heldSteerToken, id, rows),
      );
      if (lockResult === "stale") steerStale = true;
    } catch {
      // Lock/lease failure: never claim the interrupt (the bounded retry /
      // reconnect delivers the same id later). Attribute the failed turn
      // against the CAPTURED box, then run the shared epilogue.
      heldSteerToken.box.noteDeliveryFailure(instanceId);
      steerFailed = true;
    }
    // Gate 36 item 5: the turn's identity was replaced mid-lock; return before
    // reading current-box state (this.outbox may be null).
    if (steerStale) return false;
    await heldSteerToken.box.deliveryEpilogue().catch(() => undefined);
    if (steerFailed) return false;
    this.syncBubbleFromOutbox(commandId);
    // Receipt only on AUTHORITATIVE acceptance (sent/done), never while merely
    // queued/reconciling/offline.
    const finalState = heldSteerToken.box.get(commandId)?.state;
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
    // The store's jittered retry owns this instance's next delivery; the
    // outbox must not arm a parallel delivery wake from a flush epilogue.
    this.outbox?.noteStoreRetryScheduled(instanceId);
    const timer = setTimeout(() => {
      this.outboxRetryTimer.delete(instanceId);
      this.outbox?.clearStoreRetry(instanceId);
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
    this.outbox?.clearStoreRetry(instanceId);
  }

  /** Serialise reconciliation jobs for one instance (FIFO, error-tolerant). */
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
    extras?: {
      model?: string;
      effort?: EffortSelection;
      /** c-effortread r9 item 6: explicit axis intent carried on effort.axis. */
      effortAxis?: EffortAxis;
      /** c-effortread r10: per-request id echoed on lifecycle/echo edges. */
      effortRequestId?: string;
    },
    freshness?: { effortNonce?: number },
  ) {
    // D-056 wire: `{name, ultracode, index, axis, requestId}` — a native level
    // word plus the orthogonal boolean, the explicit axis the driver executes
    // and the per-request id the driver echoes on its edges; the web never
    // sends the legacy "ultracode" name.
    const wireExtras = extras?.effort
      ? {
          ...extras,
          effort: {
            name: effortWireName(extras.effort),
            ultracode: extras.effort.ultracode === true,
            index: extras.effort.index,
            // c-effortread r9 item 6: state the axis so a cold flag click is
            // the toggle the driver types ("ultracode on"), never a substituted
            // tier command.
            ...(extras.effortAxis ? { axis: extras.effortAxis } : {}),
            // c-effortread r10: carry the per-request id to the driver.
            ...(extras.effortRequestId ? { requestId: extras.effortRequestId } : {}),
          },
        }
      : extras;
    await api.instanceConfigure(instanceId, permissionMode, wireExtras);
    // c-effortui r2 item 2: a request's HTTP success lands AFTER the user
    // replaced it with a newer request must not overwrite the newer choice.
    // The effort post-emit is gated on the captured request nonce; the
    // authoritative projection is folded by the caller's bounded refresh.
    const effortStillCurrent =
      freshness?.effortNonce == null ||
      this.state.effortPending[instanceId]?.nonce === freshness.effortNonce;
    this.emit({
      permissionMode: { ...this.state.permissionMode, [instanceId]: permissionMode },
      ...(extras?.effort && effortStillCurrent
        ? { effort: { ...this.state.effort, [instanceId]: extras.effort } }
        : {}),
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

  /**
   * Apply the live-session version gate to a requested selection BEFORE it is
   * shown or posted (D-056 §3/§5):
   *  - coupled (2.1.203–2.1.283): a flag-only switch ON (same tier) forces the
   *    slider to xhigh and posts one `/effort ultracode`; a tier drag that
   *    leaves xhigh switches the flag OFF, and `{non-xhigh, true}` is therefore
   *    never produced.
   *  - legacy/unknown: the boolean fails closed (the switch is locked in the
   *    UI; never post true on the strength of an unreported version).
   *  - decoupled (≥2.1.284): the two axes stay orthogonal.
   */
  private gateLiveEffort(instanceId: Id, requested: EffortSelection): EffortSelection {
    const instance = this.state.instances.find((row) => row.id === instanceId);
    if ((instance?.kind ?? "claude") !== "claude") return requested;
    const gate = this.effortVersionGate(instanceId);
    if (gate === "coupled") {
      // c-effortui r4 item 4: read the CURRENT selection from the same source
      // the UI renders (`effortOf`), not the optimistic-only `state.effort`
      // map, which is empty after a reload or on another device. With a
      // persisted record {xhigh,true} a drag to max otherwise saw no current
      // selection and re-linked the switch to xhigh, swallowing the first drag
      // and re-typing `/effort ultracode`.
      const current = this.effortOf(instanceId, "claude");
      const currentTier = current.name;
      // Same tier as the current selection with the flag on = the switch
      // itself was flipped → link it to xhigh. When the current tier is
      // unknown (no record and no optimistic choice) the first flag-on links
      // too, preserving the cold-start switch behaviour.
      const hasRecord =
        Boolean(this.state.effort[instanceId]) ||
        Boolean(instance?.effortName) ||
        Boolean(this.state.effortEffective[instanceId]);
      if (requested.ultracode === true && (!hasRecord || currentTier === requested.name)) {
        return coupledSelection(requested);
      }
      // A tier move away from xhigh: the coupled flag exists only at xhigh,
      // so the flag is OFF no matter what the caller's optimistic axes carried
      // (e.g. a drag whose input still rides the flag from the old tier).
      if (requested.name !== "xhigh" || requested.index !== CLAUDE_XHIGH_INDEX) {
        return { ...requested, ultracode: false };
      }
      return requested;
    }
    if ((gate === "legacy" || gate === "unknown") && requested.ultracode === true) {
      return { ...requested, ultracode: false };
    }
    return requested;
  }

  /**
   * c-effortread r12 item 5 / r19 item 1: arm the bounded, request-owned
   * reconciliation for one client configure. A current driver settles the
   * request from its echoed id well before the bound, at which point the timer
   * fires into a no-op. A request whose own id evidence never arrives (lost
   * frames, provenance failure) needs this path: it is never settled by an
   * id-less projection and ends explicitly unconfirmed.
   */
  private armEffortReconciliation(instanceId: Id, nonce: number) {
    // A replaced request's configure can resolve AFTER the newer request
    // armed its own slot: never let a stale nonce overwrite it.
    const existing = this.effortReconcileTimers.get(instanceId);
    if (existing && existing.nonce > nonce) return;
    if (existing) clearTimeout(existing.timer);
    const timer = setTimeout(() => {
      void this.runEffortReconciliation(instanceId, nonce);
    }, EFFORT_UNCONFIRMED_BOUND_MS);
    this.effortReconcileTimers.set(instanceId, { nonce, timer });
  }

  /**
   * Bound expiry for one effort request. The protocol has no operation to
   * query the driver's current effort (r13 item 4), so this never re-reads
   * anything: a genuine post-command projection carrying the request's OWN id
   * arrives over the live journal or the routine poll and settles before the
   * deadline (r19 item 1: an id-less projection settles nothing); otherwise
   * the request ends in the explicit "unconfirmed" outcome. The bound is
   * request-owned FROM THE MOMENT THE REQUEST COULD RUN (r13 item 2/3 — a
   * client request at the working→idle evidence, a wire pending at receipt
   * when idle): a request still behind a running turn
   * (`leftQueueAt == null`) waits instead of burning its bound in the queue;
   * the 30-minute max-age backstop bounds the total wait.
   */
  private runEffortReconciliation(instanceId: Id, nonce: number) {
    const slot = this.effortReconcileTimers.get(instanceId);
    const pendingNow = this.state.effortPending[instanceId];
    // Settled/failed, or replaced by a newer request (which armed its own
    // slot): never touch another request's pending.
    if (!pendingNow || pendingNow.nonce !== nonce) {
      if (slot?.nonce === nonce) this.effortReconcileTimers.delete(instanceId);
      return;
    }
    if (pendingNow.leftQueueAt == null) {
      // Still behind the running turn: no read-back is even possible yet.
      // Wait; the working→idle activity evidence stamps leftQueueAt and
      // re-arms a fresh full bound. Never refresh and never declare
      // unconfirmed while queued. The 30-minute max-age is the total-wait
      // backstop: past it (a genuinely lost activity edge) stop rescheduling —
      // `effortPendingOf` already prunes the chip at the same age.
      if (Date.now() - pendingNow.at > EFFORT_PENDING_MAX_AGE_MS) {
        this.effortReconcileTimers.delete(instanceId);
        return;
      }
      const timer = setTimeout(() => {
        void this.runEffortReconciliation(instanceId, nonce);
      }, EFFORT_UNCONFIRMED_BOUND_MS);
      this.effortReconcileTimers.set(instanceId, { nonce, timer });
      return;
    }
    // The request owns its bound from when it could run (an idle-send request
    // initializes that to the send time). A timer that was armed BEFORE a late
    // activity exit must not fire early: reschedule the remainder instead of
    // reconciling on a partial window.
    const boundStart = pendingNow.leftQueueAt;
    const elapsed = Date.now() - boundStart;
    if (elapsed < EFFORT_UNCONFIRMED_BOUND_MS) {
      const timer = setTimeout(() => {
        void this.runEffortReconciliation(instanceId, nonce);
      }, EFFORT_UNCONFIRMED_BOUND_MS - elapsed);
      this.effortReconcileTimers.set(instanceId, { nonce, timer });
      return;
    }
    // c-effortread r13 item 4 / coordinator ruling: the protocol offers NO
    // request the web/Hub can send to obtain the driver's CURRENT effort (the
    // hub-node method set has configure/send/read for other surfaces, nothing
    // that forces a driver read-back), so the bound must not re-read the Hub
    // database and present stored state as a FRESH read-back. Genuine driver
    // projections produced after the command still reach this store through
    // the normal channels — the live follow effort edge and the routine poll —
    // and settle via the strict fallback BEFORE this instant. If the pending
    // is still here, no fresh evidence ever arrived: end the request.
    const after = this.state.effortPending[instanceId];
    if (!after || after.nonce !== nonce) {
      this.effortReconcileTimers.delete(instanceId);
      return;
    }
    if (after.leftQueueAt == null) {
      // A new turn re-queued the request during the bound: the next
      // working→idle evidence re-arms it; wait rather than ending it
      // unconfirmed against a request the driver has not run yet.
      const timer = setTimeout(() => {
        void this.runEffortReconciliation(instanceId, nonce);
      }, EFFORT_UNCONFIRMED_BOUND_MS);
      this.effortReconcileTimers.set(instanceId, { nonce, timer });
      return;
    }
    // No fresh evidence within the bound: stop the silent 切换中 explicitly.
    // Keep the optimistic selection — there is no refusal evidence, and an
    // accepted no-op targeted the observed state — and say so in a toast.
    const pendingNext = { ...this.state.effortPending };
    delete pendingNext[instanceId];
    this.settledEffortPushdown.delete(instanceId);
    this.emit({ effortPending: pendingNext });
    this.effortReconcileTimers.delete(instanceId);
    this.toast(
      `effort 切换结果未确认：驱动未在时限内回读 ${after.name}${after.ultracode ? " · ultracode" : ""}，已保留所选档位，请核对实际状态`,
    );
  }

  async setEffort(instanceId: Id, effortIn: EffortSelection, axis: EffortAxis = "tier") {
    const effort = this.gateLiveEffort(instanceId, effortIn);
    const instance = this.state.instances.find((row) => row.id === instanceId);
    // r15 item 1: waiting-interaction (PermissionRequest/Elicitation) is just
    // as unrunnable as working — the driver needs Idle/Done +
    // interactive_ready — so the request is queued and its deadline starts
    // only from authoritative idle evidence.
    const busy = effortAwaitsQueueExit(instance?.activity);
    // New per-request provenance (c-effortui): every switch gets a fresh
    // nonce and its OWN threshold stamped from the current projection. A
    // replaced request's late events cannot settle this indicator.
    const nonce = (this.state.effortNonces[instanceId] ?? 0) + 1;
    // c-effortread r13 item 1: a request that replaces another in flight makes
    // every later id-less word-only queued edge ambiguous.
    const afterReplacement = Boolean(this.state.effortPending[instanceId]);
    // c-effortread r10 items 1–4: the per-request id that rides the configure
    // to the driver and returns on lifecycle edges and the requested echo.
    const requestId = id("effreq_") as unknown as string;
    const currentView = this.state.effortEffective[instanceId] ?? null;
    // c-effortui r4 item 3: predict the exact word the driver will type so the
    // queued/applied/degraded lifecycle (which echoes the typed command word)
    // can be matched even when a decoupled TIER move carries the flag — the
    // driver then types the plain level word with flag=true in the payload.
    const commandWord = predictDriverCommandWord(effort, axis, instance, this.state, instanceId);
    const now = Date.now();
    // The coupled bare `ultracode` word moves BOTH axes. The decoupled toggle
    // moves only the flag and a plain tier word only the level (r10 item 4:
    // the other axis is driven by its OWN configure, so axes this request
    // does not move start settled rather than waiting for another request's
    // read-back).
    const movesBoth = commandWord === "ultracode";
    const movesLevel = movesBoth || axis !== "flag";
    const movesFlag = movesBoth || axis === "flag";
    const pending: EffortPending = {
      nonce,
      requestId,
      axis,
      name: effort.name,
      ultracode: effort.ultracode === true,
      queued: busy,
      at: now,
      // r13 item 2: an idle-sent request starts its reconciliation bound at
      // the send time; a queued one starts it when activity shows it leaving
      // the queue.
      leftQueueAt: busy ? null : now,
      // The pre-request baseline is kept SEPARATE from the queued lifecycle
      // time (r10 item 1): an id-less read-back settles only when newer than
      // BOTH and matching the requested axis.
      baselineObservedAt: currentView?.observedAt ?? null,
      queuedObservedAt: null,
      // Client pendings order via the nonce/afterReplacement protection, not
      // a lifecycle seq (r16 item 5).
      queuedSeq: null,
      // Axes this request does not move need no proof; every MOVED axis
      // starts unsettled even when the projection already shows the target
      // (c-effortread r11 item 2): pre-settling let a queued B that targeted
      // the old observation clear itself on A's mismatching-id echo. An
      // accepted no-op switch settles from its own request-id echo.
      levelSettled: !movesLevel,
      flagSettled: !movesFlag,
      source: "client",
      commandWord,
      afterReplacement,
      // Per-session capability, carried across requests (r18 item 2 / r19
      // item 1): an unknown session fails closed on id-less evidence and
      // settles only on this request's own id echo; only a proven id-capable
      // session is marked as such up front.
      idCapability: this.effortIdCapability(instanceId),
    };
    this.settledEffortPushdown.delete(instanceId);
    // Optimistically adopt the requested axes SYNCHRONOUSLY, before the
    // configure POST resolves: a second switch (e.g. the ultracode switch
    // flipped right after a tier drag) must read the freshest requested tier
    // from effortOf(), not the pre-POST value (D-056 rapid level→flag race).
    this.emit({
      effortNonces: { ...this.state.effortNonces, [instanceId]: nonce },
      effortPending: { ...this.state.effortPending, [instanceId]: pending },
      effort: { ...this.state.effort, [instanceId]: effort },
    });
    try {
      await this.configure(
        instanceId,
        this.permissionModeOf(instanceId),
        { effort, effortAxis: axis, effortRequestId: requestId },
        { effortNonce: nonce },
      );
      // Re-request authoritative state once the command is accepted. The
      // driver projects the read-back before acking, so folding the durable
      // record settles the chip from the Hub even when this client's live
      // follow frame was gapped/coalesced; the 2s poll keeps settling it if
      // this read races. Bounded by one network round trip — no timer.
      await this.refresh().catch(() => undefined);
      // c-effortread r12 item 5 / r13 item 4: arm the bounded reconciliation.
      // A new driver's id-bearing completion settles the request long before
      // the bound; an id-less driver's silent accepted no-op gets no extra
      // query (the protocol has none) — a genuinely newer projection still
      // arrives over the live journal or routine poll, otherwise the bound
      // ends it in an explicit unconfirmed outcome instead of sitting on
      // 切换中 for the 30-minute max age.
      this.armEffortReconciliation(instanceId, nonce);
    } catch (error) {
      // A failed configure (offline node, network, 4xx) must not leave an
      // optimistic tier/flag, an unhandled rejection, or a stuck indicator.
      // Roll back ONLY when this is still the current request: a later switch
      // has already replaced it, and reverting would clobber the newer choice.
      const stillCurrent =
        (this.state.effortPending[instanceId]?.nonce ?? -1) === nonce;
      const nextPatch: Partial<HubState> = {};
      if (stillCurrent) {
        // The pending indicator belongs to THIS request — clear it, and roll
        // the optimistic axes back to the last observed state. A replaced
        // request's late failure must not touch B's 切换中 indicator
        // (c-effortui r3 item 8).
        nextPatch.effortPending = { ...this.state.effortPending };
        delete nextPatch.effortPending[instanceId];
        const effective = this.state.effortEffective[instanceId];
        const effState = { ...this.state.effort };
        if (effective) {
          const inst = this.state.instances.find((row) => row.id === instanceId);
          const kind = (inst?.kind ?? "claude") as EffortKind;
          const reverted = effortFromRecord(
            kind,
            effective.name,
            null,
            effective.ultracode === true,
          );
          if (reverted) effState[instanceId] = reverted;
          else delete effState[instanceId];
        } else {
          delete effState[instanceId];
        }
        nextPatch.effort = effState;
        this.emit(nextPatch);
      }
      // A replaced request still gets its failure surfaced (no silent loss),
      // but the toast never names the newer in-flight request as failed.
      this.toast(
        stillCurrent
          ? `effort 切换失败：${error instanceof Error ? error.message : String(error)}`
          : `上一次 effort 切换失败：${error instanceof Error ? error.message : String(error)}`,
      );
      // Do NOT rethrow: the UI already shows the failure via the toast and the
      // rolled-back state; an unhandled promise rejection would crash tests.
    }
  }

  /**
   * Flip ONLY the ultracode switch axis, leaving the tier untouched on
   * decoupled builds (D-056 ≥2.1.284). On coupled builds the gate linkage in
   * {@link gateLiveEffort} moves the slider to xhigh when the switch turns on
   * (one `/effort ultracode`); on legacy/unknown builds the flag fails closed.
   */
  async setUltracode(instanceId: Id, ultracode: boolean) {
    const current = this.effortOf(instanceId);
    // c-effortread r9 item 6: the switch moves ONLY the flag axis.
    await this.setEffort(instanceId, { ...current, ultracode }, "flag");
  }

  /** Claude Code version gate for a session.
   *  Source order (c-effortui r2 item 1): the per-instance capability
   *  snapshot's reported binaryVersion (transcript/launch read-back), then the
   *  host's PINNED claude CLI inventory version, then unknown (which disables
   *  only the ultracode switch with a named reason). Never a fabricated
   *  version. */
  effortVersionGate(instanceId: Id): ClaudeVersionGate {
    const instance = this.state.instances.find((row) => row.id === instanceId);
    // c-effortui r4 item 5(c) / r5 item 3: the shell-pty CARRIER (promoted or
    // generic) cannot type the version-gated ultracode words — only the Herdr
    // claude_pty carrier accepts live flag-on configure (its driver refuses
    // them for shell-pty). Key on the driver, not isGenericPty (which returns
    // false for promoted shells and for shell-pty with a structured tier,
    // leaving the switch enabled on a carrier that rejects every flag-on POST).
    if (instance?.driver === "shell-pty") return "unknown";
    const reported = instance?.capabilities?.binaryVersion;
    if (reported) return claudeVersionGate(reported);
    if (instance) {
      const host = this.state.hosts.find((row) => row.id === instance.hostId);
      const pinned = hostClaudeVersion(host);
      if (pinned) return claudeVersionGate(pinned);
    }
    return claudeVersionGate(null);
  }

  /** Per-session switch refusal, applying the model-scoped expiry (D-056 §5). */
  effortRefusalOf(instanceId: Id): UltraRefusal | null {
    const refusal = this.state.effortRefusal[instanceId];
    if (!refusal) return null;
    if (refusal.scope === "model") {
      // Bound to the model the driver named; a different current model
      // re-enables the switch immediately.
      const currentModel = this.currentEffortModelId(instanceId);
      if (currentModel && refusal.modelId && currentModel !== refusal.modelId) {
        return null;
      }
    }
    return refusal;
  }

  /** Static capability rows (per-model default + ultracode), Hub or null. */
  supplyCatalogRows(): ModelEffortCatalogRow[] | null {
    return this.state.supplyCatalog;
  }

  /** Fetch GET /v1/supply/catalog once (best-effort; fallback rows live in the
   *  effort module). */
  async loadSupplyCatalog(): Promise<void> {
    if (this.state.supplyCatalog) return;
    try {
      const body = await api.supplyCatalog();
      const rows = Array.isArray(body?.models)
        ? (body.models as ModelEffortCatalogRow[]).filter((row) => row && typeof row.id === "string")
        : [];
      if (rows.length) this.emit({ supplyCatalog: rows });
    } catch {
      // Operator/network unavailable: keep the built-in fallback table.
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

  /**
   * c-cardsettle r2 item 4: pin a Hub settlement (instance ended → card
   * invalidated) against the list seq captured at frame receipt, BEFORE the
   * settlement refresh starts. An older in-flight interaction poll that
   * resolves afterwards carries an older reqSeq and a stale pending copy; the
   * pin makes mergeInteractionSnapshots suppress it and keeps the tombstone
   * until a newer page confirms. The local row is flipped immediately so the
   * card drops even before the refresh resolves.
   */
  private pinHubSettlement(interactionId: Id, reason?: string) {
    this.settledInteractions.set(interactionId, {
      seq: this.listReqSeq,
      confirmedByNewer: false,
    });
    if (this.state.interactions.some((row) => row.id === interactionId)) {
      this.emit({
        interactions: this.state.interactions.map((row) => {
          if (row.id !== interactionId) return row;
          // c-cardsettle r3 item 7: stamp the terminal resolution on the
          // immediate projection too. r6 item 5: carry the Hub's reason
          // verbatim — a non-process-end settlement (e.g. transcript-picker
          // demotion = agent-demoted) must not be overwritten with
          // generation-ended; absent/unknown reasons default to generation-
          // ended (the only settlement the process-end paths publish).
          const now = new Date().toISOString();
          const settlementReason: InteractionResolutionReason =
            reason === "agent-demoted" ? "agent-demoted" : "generation-ended";
          return {
            ...row,
            state: "invalidated" as const,
            answerable: false,
            blocking: false,
            updatedAt: now,
            resolution: {
              state: "known" as const,
              value: { reason: settlementReason, eventIds: [] },
            },
          };
        }),
      });
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
    // c-effortui r2 item 4: restore BOTH the level and the durable flag from
    // the Hub record when no live selection is present (e.g. after a reload
    // whose effort event has aged out of the journal tail).
    const recorded = effortFromRecord(
      fallbackKind,
      instance?.effortName,
      instance?.effortIndex,
      instance?.effortUltracode === true,
    );
    if (recorded) return recorded;
    return effortAt(fallbackKind, readDeviceSettings().defaultEffortIndex ?? DEFAULT_EFFORT_INDEX);
  }

  /** §9.1: transcript-read-back effective effort, or `null` when unobserved. */
  effortEffectiveOf(instanceId: Id): EffortEffectiveView | null {
    return this.state.effortEffective[instanceId] ?? null;
  }

  /** r7 item 4(a): true only after an explicit read-back-unavailable edge (or
   *  a Hub projection going from a level to null). A session that simply never
   *  read back a level returns false, so a pending chip there shows the
   *  pending word rather than a false "?". */
  effortReadbackWithdrawnOf(instanceId: Id): boolean {
    return this.state.effortReadbackWithdrawn[instanceId] != null;
  }

  /** c-effortread r8 item 1: the withdrawal watermark observedAt for an
   *  instance, or null when read-back is currently available. */
  effortWithdrawnAtOf(instanceId: Id): string | null {
    return this.state.effortReadbackWithdrawn[instanceId] ?? null;
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

  /** c-effortread r9 e2e seam: resolve (now if already live) when the follow
   *  socket for `instanceId` has delivered its first contiguous live batch past
   *  the seed cursor. Returns the acked seq. Inert without the e2e marker. */
  waitForFollowCaughtUpForTest(instanceId: Id, timeoutMs = 10_000): Promise<string> {
    if (!e2eSeamsEnabled()) return Promise.resolve("");
    const already = this.followCaughtUpSeq.get(instanceId);
    if (already !== undefined) return Promise.resolve(already);
    return new Promise((resolve, reject) => {
      const timer = setTimeout(
        () => reject(new Error(`follow not caught up for ${instanceId}`)),
        timeoutMs,
      );
      const set = this.followCaughtUp.get(instanceId) ?? new Set();
      set.add((seq) => {
        clearTimeout(timer);
        resolve(seq);
      });
      this.followCaughtUp.set(instanceId, set);
    });
  }

  /** Internal: fire and remember the caught-up seam for an instance. */
  private resolveFollowCaughtUp(instanceId: Id, seq: string) {
    if (!e2eSeamsEnabled()) return;
    if (!this.followCaughtUpSeq.has(instanceId)) {
      this.followCaughtUpSeq.set(instanceId, seq);
      const waiters = this.followCaughtUp.get(instanceId);
      this.followCaughtUp.delete(instanceId);
      for (const resolve of waiters ?? []) resolve(seq);
    }
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
