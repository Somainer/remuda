import { useEffect, useReducer, useRef, useState } from "react";
import { nowMs } from "../../lib/clock";
import { hubStore, type RefreshHealth } from "../../lib/store";

/**
 * home.md §8 新鲜度 — the product constants, in one place. Gated tests assert
 * the rules through the injected clock (web/src/lib/clock.ts), never by
 * sleeping on wall-clock milliseconds.
 */
export const HOME_FRESHNESS = {
  /** The list poll period (store.startPoll). */
  pollMs: 2_000,
  /** No successful refresh within this window = stale (≈5 poll periods). */
  staleAfterMs: 10_000,
  /** A banner needs this long of CONSECUTIVE failures (4 s does not show it). */
  bannerAfterMs: 5_000,
  /** §4.4 answer receipt acknowledgement. */
  receiptMs: 2_000,
} as const;

/**
 * - `unknown`: no snapshot from the Hub yet (before the first successful read).
 * - `grace`: the page just returned to the foreground; not stale until the
 *   first refresh after it settles, whether that refresh succeeds or fails.
 * - `fresh`: the link is live and the last successful read is at most 10 s old.
 * - `stale`: everything else — cached rows are greyed and stale-only actions
 *   are held until the next success.
 */
export type HomeFreshnessState = "fresh" | "stale" | "grace" | "unknown";

export type HomeFreshness = {
  state: HomeFreshnessState;
  lastOkAt: number | null;
  failingSince: number | null;
};

function deriveState(
  health: RefreshHealth,
  connection: string,
  grace: boolean,
  now: number,
): HomeFreshnessState {
  if (health.lastOkAt === null) return "unknown";
  if (grace) return "grace";
  if (connection === "live" && now - health.lastOkAt <= HOME_FRESHNESS.staleAfterMs) return "fresh";
  return "stale";
}

/**
 * The identity that warrants a caller re-render. While fresh the timestamp is
 * left out: a quiet successful poll moves `lastOkAt` every 2 s and must NOT
 * commit the list (`commit:HomeList`). The failure run always participates —
 * a failure that starts or clears while the list is still fresh must reach
 * the banner — and once the list leaves fresh the timestamps join the key so
 * the still-failing refetches do not churn either.
 */
function freshnessKey(state: HomeFreshnessState, health: RefreshHealth): string {
  if (state === "fresh") return `fresh|${health.failingSince}`;
  return `${state}|${health.lastOkAt}|${health.failingSince}`;
}

/**
 * The home freshness verdict for the mounted list (home.md §8).
 *
 * The store owns the refresh timestamps (bootstrap + every settled refresh on
 * the injected clock); this hook adds the foreground grace and the time
 * window. It reports — and so re-renders its caller — only on a real
 * transition: subscriptions, a 1 s clock and the foreground event re-evaluate,
 * and the returned values are read live so `lastOkAt` is current whenever the
 * caller renders for any reason.
 */
export function useHomeFreshness(): HomeFreshness {
  const [, forceRender] = useReducer((count: number) => count + 1, 0);
  // Grace is per mounted surface: a foreground while no list is mounted has
  // nothing to protect. The state drives the render; the ref mirrors it for
  // the event callbacks and the clock tick.
  const [grace, setGrace] = useState(false);
  const graceRef = useRef(false);
  const keyRef = useRef<string | null>(null);

  const health = hubStore.getRefreshHealth();
  const state = deriveState(health, hubStore.getSnapshot().connection, grace, nowMs());
  const key = freshnessKey(state, health);

  useEffect(() => {
    graceRef.current = grace;
  }, [grace]);

  // Publish the committed key after the render lands, so an event between
  // render and effect cannot look like a transition that never happened.
  useEffect(() => {
    keyRef.current = key;
  }, [key]);

  useEffect(() => {
    const read = () => {
      const nextHealth = hubStore.getRefreshHealth();
      const nextState = deriveState(
        nextHealth,
        hubStore.getSnapshot().connection,
        graceRef.current,
        nowMs(),
      );
      const nextKey = freshnessKey(nextState, nextHealth);
      if (nextKey === keyRef.current) return;
      keyRef.current = nextKey;
      forceRender();
    };
    // Every settled refresh (success or failure) ends the foreground grace —
    // that is the "first refresh after it ends" of §8, whichever way it went.
    const offHealth = hubStore.subscribeRefreshHealth(() => {
      graceRef.current = false;
      setGrace(false);
      read();
    });
    // The connection state is the other freshness input.
    const offStore = hubStore.subscribe(read);
    const onVisibility = () => {
      if (document.visibilityState !== "visible") return;
      graceRef.current = true;
      setGrace(true);
      read();
    };
    document.addEventListener("visibilitychange", onVisibility);
    // Time alone crosses the 10 s window; a key comparison keeps a calm list
    // from committing once per second.
    const timer = window.setInterval(read, 1_000);
    read();
    return () => {
      offHealth();
      offStore();
      document.removeEventListener("visibilitychange", onVisibility);
      window.clearInterval(timer);
    };
  }, []);

  return { state, lastOkAt: health.lastOkAt, failingSince: health.failingSince };
}
