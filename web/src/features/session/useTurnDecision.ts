/**
 * The session page's one turn decision, re-projected on a clock only while
 * the answer can still change with time alone.
 *
 * `projectTurnDecision` is time-dependent through the hook tier's freshness:
 * a quiet hook tier drops to advisory after its stall budget and the screen or
 * transcript may then end the turn with no new event. The page used to drive
 * that with `useNow(true)` — a 1 Hz state update that re-rendered the whole
 * SessionPage every second, idle or not. This hook keeps the same judgement
 * but:
 *
 *  - re-projects in the same render when the events / native ref / pending
 *    inputs change;
 *  - runs a 1 s timer only while the decision is `working`, `waiting` or
 *    `unknown` — `ended` cannot un-end by time alone (freshness only decays);
 *  - sets state only when a tick's projection differs in `state`,
 *    `decidedBy` or `endedAt`, so an unchanged tick commits nothing;
 *  - stops while `document.hidden` and re-projects once on return.
 *
 * The composer's ended→idle flush keys off this decision's state, so the edge
 * still fires exactly once.
 */
import { useEffect, useMemo, useRef, useState } from "react";
import type { Observation } from "../../types/generated";
import type { NativeRef } from "../../types/nativeRef";
import { projectTurnDecision } from "./live/turnDecision";
import type { TurnDecision, TurnState } from "./live/turnEnd";

const TICK_MS = 1000;

/** States whose projection can move with the clock alone. */
const TICKING: ReadonlySet<TurnState> = new Set(["working", "waiting", "unknown"]);

type Inputs = {
  events: readonly Observation[];
  nativeRef: NativeRef | null | undefined;
  hasPending: boolean;
};

function sameDecision(a: TurnDecision, b: TurnDecision): boolean {
  return a.state === b.state && a.decidedBy === b.decidedBy && a.endedAt === b.endedAt;
}

export function useTurnDecision(
  events: readonly Observation[],
  nativeRef: NativeRef | null | undefined,
  hasPending: boolean,
): TurnDecision {
  // Bumped only when a clock tick changes the answer; it is the memo's only
  // time-driven dependency.
  const [clock, setClock] = useState(0);
  const decision = useMemo(
    () => projectTurnDecision(events, nativeRef, hasPending, Date.now()),
    // `clock` is a deliberate re-projection trigger, not read in the body.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [events, nativeRef, hasPending, clock],
  );

  const latest = useRef<{ inputs: Inputs; decision: TurnDecision }>({
    inputs: { events, nativeRef, hasPending },
    decision,
  });
  useEffect(() => {
    latest.current = { inputs: { events, nativeRef, hasPending }, decision };
  });

  const ticking = TICKING.has(decision.state);

  useEffect(() => {
    if (!ticking) return;
    let timer: ReturnType<typeof setInterval> | null = null;
    const tick = () => {
      const { inputs, decision: shown } = latest.current;
      const next = projectTurnDecision(inputs.events, inputs.nativeRef, inputs.hasPending, Date.now());
      if (!sameDecision(next, shown)) setClock((c) => c + 1);
    };
    const start = () => {
      if (timer === null && !document.hidden) timer = setInterval(tick, TICK_MS);
    };
    const stop = () => {
      if (timer !== null) {
        clearInterval(timer);
        timer = null;
      }
    };
    const onVisibility = () => {
      if (document.hidden) {
        stop();
      } else {
        tick();
        start();
      }
    };
    start();
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      stop();
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, [ticking]);

  return decision;
}
