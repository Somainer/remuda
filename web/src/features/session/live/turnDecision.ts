/**
 * Assemble the {@link turnEnd} inputs straight from the session's observation
 * list. Kept separate from the pure reducer (`turnEnd.ts`) so the reducer
 * stays testable over typed inputs while this module owns the fold wiring
 * (`phase`, `liveStatus`, `channelHealth`, the transcript tail and the screen
 * latch's `agent_status` verdict).
 */
import type { Observation } from "../../../types/generated";
import type { NativeRef } from "../../../types/nativeRef";
import { knowledgeValue } from "../../../types/command";
import { channelHealth, expectedTiersFor } from "./channelHealth";
import { livePhase } from "./phase";
import { liveStatus } from "./liveStatus";
import { lastAssistantMessageAt, turnEnd, type TurnDecision } from "./turnEnd";

const SCREEN_CHANNELS = new Set(["pty", "screen", "osc"]);

function seqOf(ev: Observation): bigint {
  try {
    return BigInt(ev.seq);
  } catch {
    return 0n;
  }
}

function parseTs(at: string | null | undefined): number | null {
  if (!at) return null;
  const t = Date.parse(at);
  return Number.isNaN(t) ? null : t;
}

/**
 * The turn's *start* anchor, read from the event list rather than from the
 * latched phase: once a `turn-ended` tag latches it overwrites the
 * prompt-accepted `since`, and a screen clear drops the spinner `since`, so
 * neither survives into the end frame. The frozen end duration is
 * `endedAt − turnStartAnchor`, which is the turn length the terminal shows —
 * not 0:00 / time-since-end. Earliest of the hook submit tag and an active
 * screen spinner's re-anchor.
 */
export function turnStartAnchor(events: readonly Observation[]): string | null {
  let best: { at: number; raw: string } | null = null;
  for (const ev of events) {
    if (ev.kind !== "lifecycle" || ev.payload.type !== "native") continue;
    const tags = ev.payload.relatedIds ?? {};
    const candidate =
      ev.payload.nativeName === "live.status" && tags.liveStatus !== "0"
        ? tags.since
        : tags.phase === "prompt-accepted"
          ? tags.since
          : null;
    const at = parseTs(candidate);
    if (at !== null && (best === null || at < best.at)) best = { at, raw: candidate! };
  }
  return best?.raw ?? null;
}

/**
 * The screen latch's own `agent_status` verdict ("idle"/"working"/"blocked"),
 * newest first. Unlike the spinner fold (`liveStatus`) this already carries
 * the blocked latch, so it — not the spinner — backs the reducer's
 * `screenBlocked` waiting signal.
 */
export function screenAgentState(events: readonly Observation[]): string | null {
  let latest: Observation | null = null;
  for (const ev of events) {
    if (ev.kind !== "lifecycle") continue;
    const payload = ev.payload;
    if (payload.type !== "native" || payload.nativeName !== "agent_status") continue;
    if (!SCREEN_CHANNELS.has(ev.source?.channel ?? "")) continue;
    if (!latest || seqOf(ev) >= seqOf(latest)) latest = ev;
  }
  if (!latest || latest.payload.type !== "native") return null;
  const value = knowledgeValue(latest.payload.status);
  return value ?? null;
}

/**
 * Project the one turn decision for a session. `nowMs` is injectable so the
 * caller's 1 Hz clock (and unit tests) drive the hook-freshness judgement.
 */
export function projectTurnDecision(
  events: readonly Observation[],
  nativeRef: NativeRef | null | undefined,
  hasPending: boolean,
  nowMs: number = Date.now(),
): TurnDecision {
  const phase = livePhase(events);
  const screen = liveStatus(events);
  const tiers = expectedTiersFor(nativeRef);
  const health = channelHealth(events, tiers, nowMs);
  const hookHealth = health.get("hook");
  const lastAssistantAt = lastAssistantMessageAt(events);
  const screenBlocked = screenAgentState(events) === "blocked";
  return turnEnd({ phase, screen, hookHealth, hasPending, lastAssistantAt, screenBlocked });
}
