import { useEffect, useState } from "react";
import type { Host } from "../../types/instance";
import { STALE_OFFLINE_MS } from "./model";

/** The minimal facts needed to schedule an offline→stale cutoff wake-up. */
export type StaleClockHost = Pick<Host, "state" | "online" | "ssh" | "lastSeenAt">;

function isOfflinePending(host: StaleClockHost): boolean {
  if (host.online === true || host.state === "online" || host.state === "enrolled") return false;
  if (host.ssh || host.state === "connecting") return false;
  return true;
}

/**
 * Local invalidation at the nearest offline→stale cutoff (c-perffu r3/r4).
 *
 * `isStaleOffline` is evaluated during render against the current instant
 * with a STRICT `now - lastSeen > STALE_OFFLINE_MS`, and equal quiet hosts
 * polls no longer emit a store update — without a scheduled wake-up a host
 * crossing the 30-minute cutoff while the page stayed open never moved into
 * the 状态待确认 / stale group.
 *
 * The hook keeps ONE timer armed for the earliest STILL-FUTURE
 * `lastSeenAt + STALE_OFFLINE_MS` among offline, non-SSH hosts:
 *  - it wakes with an instant strictly PAST the cutoff (cutoff + 1, never
 *    less than Date.now()) so the strict-greater stale test flips at once;
 *  - the effect depends on nowMs too, so after each firing it re-evaluates
 *    and arms the next host's cutoff (or stops), even when every poll
 *    payload is equal;
 *  - changing hosts data tears down and re-arms the single timer; unmount
 *    clears it.
 */
export function useStaleCutoffTick(hosts: readonly StaleClockHost[]): number {
  const [nowMs, setNowMs] = useState(() => Date.now());
  useEffect(() => {
    // Future relative to the LAST tick, not mount time: once a cutoff has
    // fired it must stop being a candidate so the next one gets armed.
    let next = Number.POSITIVE_INFINITY;
    for (const host of hosts) {
      if (!isOfflinePending(host)) continue;
      if (!host.lastSeenAt) continue; // already stale; isStaleOffline returns true
      const cutoff = Date.parse(host.lastSeenAt) + STALE_OFFLINE_MS;
      if (Number.isFinite(cutoff) && cutoff > nowMs) next = Math.min(next, cutoff);
    }
    if (!Number.isFinite(next)) return;
    // Strictly past the cutoff: isStaleOffline uses `now - at > cutoff`.
    const delay = Math.max(0, next + 1 - Date.now());
    const id = setTimeout(() => setNowMs(Math.max(Date.now(), next + 1)), delay);
    return () => clearTimeout(id);
  }, [hosts, nowMs]);
  return nowMs;
}
