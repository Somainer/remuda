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
 * Local invalidation at the nearest offline→stale cutoff (c-perffu r3).
 *
 * `isStaleOffline` is evaluated during render against the current instant,
 * and equal quiet hosts polls no longer emit a store update — without a
 * scheduled wake-up a host that crossed the 30-minute cutoff while the page
 * stayed open never moved into the 状态待确认 / stale group.
 *
 * The hook arms ONE timer for the earliest future
 * `lastSeenAt + STALE_OFFLINE_MS` among currently-offline, non-SSH hosts
 * (already-stale hosts and hosts with no lastSeenAt need no timer), then
 * returns a fresh nowMs at the crossing so the page re-groups. It depends
 * on the hosts list: a poll that actually changes data re-arms; equal
 * payloads keep identities stable and leave the timer untouched.
 */
export function useStaleCutoffTick(hosts: readonly StaleClockHost[]): number {
  const [nowMs, setNowMs] = useState(() => Date.now());
  useEffect(() => {
    const current = Date.now();
    let next = Number.POSITIVE_INFINITY;
    for (const host of hosts) {
      if (!isOfflinePending(host)) continue;
      if (!host.lastSeenAt) continue; // already stale; isStaleOffline returns true
      const cutoff = Date.parse(host.lastSeenAt) + STALE_OFFLINE_MS;
      if (Number.isFinite(cutoff) && cutoff >= current) next = Math.min(next, cutoff);
    }
    if (!Number.isFinite(next)) return;
    // Wake at the cutoff instant itself; a separate 15s display cadence (the
    // app-wide label clocks) re-renders shortly afterwards on real systems, so
    // timer slack can never visibly strand the row in the old group.
    const id = setTimeout(() => setNowMs(next), Math.max(0, next - current));
    return () => clearTimeout(id);
  }, [hosts]);
  return nowMs;
}
