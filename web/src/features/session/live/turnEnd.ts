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
 * - a hook turn boundary (`turn-ended` / `interrupted`) is authoritative;
 * - while the hook tier is *fresh* a hook active phase holds the turn open and
 *   the screen's idle edge is ignored (design §2.4 rule 6, raise-only);
 * - once the hook tier is stalled or never-materialised it drops to advisory
 *   and the screen's inactive `live.status` ends the turn;
 * - with no screen either, an assistant message newer than the latched phase
 *   ends it from the transcript tail.
 *
 * The end is the *earliest* channel that legitimately called it, so a late
 * hook `Stop` arriving after a screen-decided end neither moves `endedAt`
 * forward nor changes `decidedBy` — the decision is idempotent over the event
 * set. `unknown` never collapses to idle and never to `等待操作`.
 *
 * Pure: `phase.ts` and `liveStatus.ts` stay untouched folds; this reducer owns
 * every consumer decision.
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

/** Precedence for a timestamp tie: hook > screen > transcript. */
const RANK: Record<DecidedBy, number> = { hook: 0, screen: 1, transcript: 2 };

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

  // --- end candidates, each with the anchor it would stop the clock at ---
  const candidates: { channel: DecidedBy; at: string }[] = [];
  if (hookEnded) {
    const at = phase!.since ?? phase!.observedAt;
    if (at) candidates.push({ channel: "hook", at });
  }
  // The screen idle edge counts only once this run expected a hook tier that
  // is no longer vouching for a live turn (advisory, or it reported the end
  // itself). That suppresses both a pure-pty spinner clear (no hook turn to
  // close) and the brief mid-turn "idle" the TUI shows between tool calls
  // (raise-only, rule 6) while still ending a stalled turn from the screen.
  const screenInactive = screen && screen.active === false && screen.observedAt;
  if (screenInactive && hookExpected && (advisory || hookEnded)) {
    candidates.push({ channel: "screen", at: screen!.observedAt! });
  }
  // No screen at all: an assistant reply newer than the latched phase is the
  // last evidence the turn produced output and then went quiet. Requires a
  // real (expected, latched) hook turn — a lone message never opens/closes one.
  if (hookExpected && phase && advisory && !screen && lastAssistantAt) {
    const at = parse(lastAssistantAt);
    if (at !== null && (phaseSince === null || at > phaseSince)) {
      candidates.push({ channel: "transcript", at: lastAssistantAt });
    }
  }
  if (candidates.length > 0) {
    candidates.sort((a, b) => {
      const ta = parse(a.at) ?? 0;
      const tb = parse(b.at) ?? 0;
      return ta !== tb ? ta - tb : RANK[a.channel] - RANK[b.channel];
    });
    const winner = candidates[0]!;
    return { state: "ended", decidedBy: winner.channel, endedAt: winner.at };
  }

  // --- still open ---
  // 等待操作 is for a real human turn only: a pending interaction for this
  // instance, the screen latch reporting blocked, or a *fresh* hook blocked
  // phase (since the map fix, only a real PermissionRequest/Elicitation hook
  // raises blocked). A blocked phase whose hook tier is stalled with nothing
  // pending is `unknown`, never waiting — the idle_prompt Notification that
  // used to latch blocked must not masquerade as a human turn.
  const freshBlocked = fresh && phase?.phase === "blocked";
  if (hasPending || screenBlocked === true || freshBlocked) {
    return { state: "waiting", decidedBy: freshBlocked ? "hook" : "screen", endedAt: null };
  }
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
