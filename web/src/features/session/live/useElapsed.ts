/**
 * The single place elapsed time exists (live-view design §2.4, §3.3).
 *
 * Elapsed is never transported: `turn.live` carries one RFC3339 `since`
 * anchor per transition and the browser renders `now − since` at 1 Hz — one
 * wire event per phase, zero per second. The clock is requestAnimationFrame-
 * throttled (so a hidden tab, whose frames the browser stops, pauses the
 * reading automatically) and greys when the channel health handed in is not
 * `ok`: the reading stays arithmetically honest ("how long since the harness
 * told us this started") while no longer claiming the phase is fresh.
 */
import { useEffect, useState, useSyncExternalStore } from "react";
import { isUnfresh, type TierHealth } from "./channelHealth";

export type Elapsed = { ms: number; text: string; stale: boolean };

/** `m:ss`, `h:mm:ss` past the first hour, clamped at zero. */
export function formatElapsed(ms: number): string {
  const total = Math.max(0, Math.floor(ms / 1000));
  const hours = Math.floor(total / 3600);
  const minutes = Math.floor((total % 3600) / 60);
  const seconds = total % 60;
  const ss = String(seconds).padStart(2, "0");
  if (hours > 0) return `${hours}:${String(minutes).padStart(2, "0")}:${ss}`;
  return `${minutes}:${ss}`;
}

/**
 * 1 Hz clock, rAF-throttled. rAF stops firing on a hidden tab, which pauses
 * the loop for free; `visibilitychange` recomputes immediately on return so a
 * long hidden stretch cannot leave a stale reading painted.
 */
export function useNow(active: boolean): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!active) return;
    let frame = 0;
    let lastTick = Date.now();
    const tick = () => {
      const t = Date.now();
      if (t - lastTick >= 1000) {
        lastTick = t;
        setNow(t);
      }
      frame = requestAnimationFrame(tick);
    };
    frame = requestAnimationFrame(tick);
    const onVisible = () => {
      if (!document.hidden) {
        lastTick = Date.now();
        setNow(lastTick);
      }
    };
    document.addEventListener("visibilitychange", onVisible);
    return () => {
      cancelAnimationFrame(frame);
      document.removeEventListener("visibilitychange", onVisible);
    };
  }, [active]);
  return now;
}

/** Elapsed since an RFC3339 anchor; `null` while there is no anchor. */
export function useElapsed(
  since: string | null | undefined,
  active: boolean,
  health?: TierHealth,
): Elapsed | null {
  const now = useNow(active);
  if (!since) return null;
  const anchor = Date.parse(since);
  if (Number.isNaN(anchor)) return null;
  return {
    ms: now - anchor,
    text: formatElapsed(now - anchor),
    stale: isUnfresh(health),
  };
}

// --- Tool-card bridge ------------------------------------------------------
//
// ToolCard sits deep inside the virtualised transcript and receives only the
// wire payload; the `since` anchor lives on the phase tags in the event list.
// Rather than thread new props through Transcript (owned by another batch) or
// keep a second clock, the mounted LiveStatusStrip publishes the projected
// tool anchors keyed by content fingerprint, so a hook running node and the
// transcript node that supersedes it tick off the same anchor even though
// their wire node ids differ. One SessionPage exists at a time; navigating
// away unmounts the strip and clears it.

type ToolLiveEntry = { since: string; tier: string };

let toolAnchorsStore = new Map<string, ToolLiveEntry>();
let healthStore = new Map<string, TierHealth>();
let storeVersion = 0;
const storeListeners = new Set<() => void>();

function emitStoreChange() {
  storeVersion += 1;
  for (const listener of storeListeners) listener();
}

/** Publish the projection the running tool cards anchor to. */
export function publishToolLive(
  anchors: ReadonlyMap<string, ToolLiveEntry>,
  health: ReadonlyMap<string, TierHealth>,
): void {
  toolAnchorsStore = new Map(anchors);
  healthStore = new Map(health);
  emitStoreChange();
}

/** Drop all anchors (session switch / strip unmount). */
export function resetToolLive(): void {
  if (toolAnchorsStore.size === 0 && healthStore.size === 0) return;
  toolAnchorsStore = new Map();
  healthStore = new Map();
  emitStoreChange();
}

function subscribeStore(listener: () => void): () => void {
  storeListeners.add(listener);
  return () => {
    storeListeners.delete(listener);
  };
}

/**
 * Ticking elapsed for a running tool card keyed by its content fingerprint.
 * Returns `null` once the card is not running or before the strip has
 * projected the tool-start anchor — the caller then paints no elapsed and,
 * more importantly, no exit code.
 */
export function useToolElapsed(
  fingerprint: string,
  running: boolean,
): { text: string; stale: boolean } | null {
  // Subscribing re-renders the card when the strip publishes the anchor; the
  // version value itself is not read.
  useSyncExternalStore(subscribeStore, () => storeVersion, () => 0);
  const now = useNow(running);
  if (!running) return null;
  const entry = toolAnchorsStore.get(fingerprint);
  if (!entry) return null;
  const anchor = Date.parse(entry.since);
  if (Number.isNaN(anchor)) return null;
  const stale = isUnfresh(healthStore.get(entry.tier));
  return { text: formatElapsed(now - anchor), stale };
}
