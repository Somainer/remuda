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
 * The current turn's *start* anchor, read from the event list rather than from
 * the latched phase: once a `turn-ended` tag latches it overwrites the
 * prompt-accepted `since`, and a screen clear drops the spinner `since`, so
 * neither survives into the end frame. The frozen end duration is
 * `endedAt − turnStartAnchor`, the turn length the terminal shows.
 *
 * The current turn begins at the *latest* prompt-accepted (an earlier one
 * belongs to a prior turn); an active spinner re-anchor at/after that submit
 * only ever ties or moves it earlier. A screen-only session has no submit tag,
 * so fall back to its latest active spinner anchor.
 */
export function turnStartAnchor(events: readonly Observation[]): string | null {
  let promptSince: string | null = null;
  let promptAt: number | null = null;
  let screenSince: string | null = null;
  let screenAt: number | null = null;
  for (const ev of events) {
    if (ev.kind !== "lifecycle" || ev.payload.type !== "native") continue;
    const tags = ev.payload.relatedIds ?? {};
    if (tags.phase === "prompt-accepted") {
      const at = parseTs(tags.since);
      if (at !== null && (promptAt === null || at >= promptAt)) {
        promptAt = at;
        promptSince = tags.since ?? null;
      }
    } else if (ev.payload.nativeName === "live.status" && tags.liveStatus !== "0") {
      const at = parseTs(tags.since);
      if (at !== null && (screenAt === null || at >= screenAt)) {
        screenAt = at;
        screenSince = tags.since ?? null;
      }
    }
  }
  if (promptSince !== null) return promptSince;
  return screenSince;
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
