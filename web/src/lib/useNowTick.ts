import { useEffect, useState } from "react";

/**
 * Local relative-time clock for surfaces that paint `formatListTime` labels
 * (刚刚 / Nm / 昨天…). It re-renders the calling component every intervalMs
 * with a fresh Date.now(), INDEPENDENT of store emissions — identity-stable
 * quiet polls (c-perffu: unchanged snapshots no longer emit) must not freeze
 * those labels. 30 s is fine for formatListTime's 45 s / minute boundaries;
 * a tick within 30 s after every crossing keeps the painted label honest.
 *
 * Keep this clock out of any store/snapshot path: its sole job is to drive
 * display labels, and getSnapshot() must stay referentially stable.
 */
export function useNowTick(intervalMs = 30_000): number {
  const [nowMs, setNowMs] = useState(() => Date.now());
  useEffect(() => {
    const timer = window.setInterval(() => setNowMs(Date.now()), intervalMs);
    return () => window.clearInterval(timer);
  }, [intervalMs]);
  return nowMs;
}
