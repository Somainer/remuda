/**
 * `turnEnd` — the one place the UI decides whether a turn is still open.
 *
 * The strip's phase (`phase.ts`) is a pure fold of hook lifecycle tags and it
 * *latches*: with no `Stop` hook it never unlatches, so a session whose hook
 * channel went quiet (a degraded relay, a stalled journal pump) stays stuck on
 * the last phase — `等待操作`, clock running — even though the terminal and the
 * screen tier both say the turn finished. This reducer folds every channel the
 * session already has, exactly as the Node's worker classifier merges them, so
 * the end of a turn is decided from evidence rather than from one latch:
 *
 * Precedence, in order:
 *
 * 1. A real human turn wins over **every** end signal. A parked permission hook
 *    sends nothing by definition and the spinner clears while its dialog is up,
 *    so a pending interaction (or the screen latch's own `blocked` verdict, or a
 *    fresh hook `blocked` phase) keeps the turn `waiting` no matter how quiet the
 *    hook tier gets or what the spinner currently shows.
 * 2. A hook turn boundary (`turn-ended` / `interrupted`) wins outright, fresh or
 *    not — a boundary event is terminal; it is the harness's own word.
 * 3. While the hook tier is *fresh* a hook active phase holds the turn open and
 *    the screen's idle edge is ignored (design §2.4 rule 6, raise-only).
 * 4. Once the hook tier is stalled or never-materialised it drops to advisory
 *    and the screen's inactive `live.status` ends the turn. The clear is emitted
 *    once per leave and its timestamp survives into the next turn, so it only
 *    counts when it is at or after the latched phase's `since`.
 * 5. With no screen either, an assistant message newer than the latched phase
 *    ends it from the transcript tail.
 *
 * `unknown` never collapses to idle and never to `等待操作`.
 *
 * Pure: `phase.ts` and `liveStatus.ts` stay untouched folds; this reducer owns
 * every consumer decision. The decision is rendered once and the composer maps
 * `ended → idle`; that working→idle edge fires the held-queue flush a single
 * time, so a hook Stop landing after a screen-decided end (it overrides
 * `decidedBy` here per rule 2) neither re-flushes nor moves the duration clock —
 * the elapsed reading anchors at the turn *start*, not at `endedAt`.
 */
import type { Observation } from "../../../types/generated";
import type { TierHealth } from "./channelHealth";
import type { ScreenLiveStatus } from "./liveStatus";
import type { LivePhase } from "./phase";

export type TurnState = "working" | "waiting" | "ended" | "unknown";
export type DecidedBy = "hook" | "screen" | "transcript";

export type TurnDecision = {
  state: TurnState;
  /** Which channel decided the end; only meaningful on `ended`. */
  decidedBy: DecidedBy | null;
  /** RFC3339-ms anchor of the end, `null` unless `ended`. */
  endedAt: string | null;
};

export type TurnEndInput = {
  /** The latched hook phase (`phase.ts`), or `null` when none. */
  phase: LivePhase | null;
  /** The screen-tier spinner fold (`liveStatus.ts`), or `null`. */
  screen: ScreenLiveStatus | null;
  /** Health of the hook tier (`channelHealth.ts`); `undefined` = not expected. */
  hookHealth: TierHealth | undefined;
  /** A real dialog/permission is pending for this instance (`SessionPage`). */
  hasPending: boolean;
  /** RFC3339-ms of the newest assistant message, or `null`. */
  lastAssistantAt: string | null;
  /** The screen latch itself reports a blocking dialog. */
  screenBlocked?: boolean;
};

/** A hook turn boundary — the definitive end regardless of freshness. */
function phaseIsEnd(phase: LivePhase | null): boolean {
  return phase?.phase === "turn-ended" || phase?.phase === "interrupted";
}

function parse(at: string | null | undefined): number | null {
  if (!at) return null;
  const t = Date.parse(at);
  return Number.isNaN(t) ? null : t;
}

/**
 * Fold the channels to one turn decision. See the module comment for the
 * precedence rules; this is a pure function of its inputs.
 */
export function turnEnd(input: TurnEndInput): TurnDecision {
  const { phase, screen, hookHealth, hasPending, lastAssistantAt, screenBlocked } = input;
  const fresh = hookHealth?.reason === "ok";
  // The hook tier is part of this run at all. A generic-pty session has no
  // hook-relative turn, so a spinner clear there must not manufacture an end
  // (the instance projection owns idle); only an expected-but-silent tier
  // ("stalled" / "never-materialised") drops to advisory.
  const hookExpected = hookHealth !== undefined;
  const advisory = hookExpected && !fresh;
  const hookEnded = phaseIsEnd(phase);
  const phaseSince = parse(phase?.since) ?? parse(phase?.observedAt);

  // 1. A real human turn outranks every end candidate. A parked hook is silent
  //    by definition and the spinner clears behind its dialog, so without this
  //    guard a permission left open past the hook-stall budget would read as
  //    "ended" and POST the held queue into an agent still on the dialog.
  const freshBlocked = fresh && phase?.phase === "blocked";
  if (hasPending || screenBlocked === true || freshBlocked) {
    // `decidedBy` is meaningful only on `ended`.
    return { state: "waiting", decidedBy: null, endedAt: null };
  }

  // 2. A hook turn boundary is the harness's own terminal word and wins
  //    outright (a Stop that lands after a screen-decided end overrides
  //    decidedBy but the consumer's ended→idle edge stays idempotent).
  if (hookEnded) {
    const at = phase!.since ?? phase!.observedAt;
    if (at) return { state: "ended", decidedBy: "hook", endedAt: at };
  }

  // 4. The hook tier is advisory: a screen idle edge at/after this turn's phase
  //    anchor ends it. A pure-pty session (no hook tier) is excluded — its
  //    spinner clear never opens or closes a hook turn. The anchor lower bound
  //    rejects a stale clear that was emitted for the *previous* turn and never
  //    repainted during a short turn that showed no spinner of its own.
  if (advisory && screen && screen.active === false && screen.observedAt) {
    const at = parse(screen.observedAt);
    if (at !== null && (phaseSince === null || at >= phaseSince)) {
      return { state: "ended", decidedBy: "screen", endedAt: screen.observedAt };
    }
  }

  // 5. No screen at all: an assistant reply newer than the latched phase is the
  //    last evidence the turn produced output and then went quiet. Requires a
  //    real (expected, latched) hook turn — a lone message never opens one.
  if (hookExpected && phase && advisory && !screen && lastAssistantAt) {
    const at = parse(lastAssistantAt);
    if (at !== null && (phaseSince === null || at > phaseSince)) {
      return { state: "ended", decidedBy: "transcript", endedAt: lastAssistantAt };
    }
  }

  // --- still open ---
  if (fresh && phase) {
    return { state: "working", decidedBy: "hook", endedAt: null };
  }
  if (screen && screen.active === true) {
    return { state: "working", decidedBy: "screen", endedAt: null };
  }
  // Hook advisory, no live screen, nothing pending: we cannot honestly claim
  // the turn ended, and we must not fall back to the stale latch.
  return { state: "unknown", decidedBy: null, endedAt: null };
}

/** RFC3339-ms of the newest assistant message in the event list, or `null`. */
export function lastAssistantMessageAt(events: readonly Observation[]): string | null {
  let latest: string | null = null;
  let latestSeq = -1n;
  for (const ev of events) {
    if (ev.kind !== "message") continue;
    const payload = ev.payload as { role?: string };
    if (payload.role !== "assistant") continue;
    let seq: bigint;
    try {
      seq = BigInt(ev.seq);
    } catch {
      seq = 0n;
    }
    if (seq >= latestSeq) {
      latestSeq = seq;
      latest = ev.observedAt;
    }
  }
  return latest;
}
