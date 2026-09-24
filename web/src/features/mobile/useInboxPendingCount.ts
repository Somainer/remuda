import { useEffect, useMemo, useSyncExternalStore } from "react";
import { useHub } from "../../lib/store";
import { thisDeviceId } from "../../lib/interactionStatus";
import { deriveInboxQueue } from "./inboxRows";
import {
  getInboxClockNow,
  subscribeInboxClock,
  syncInboxDeadlineClock,
} from "./inboxClock";

/**
 * Re-render the caller and provide the current clock instant whenever a known
 * interaction deadline crosses with no store emission (c-ghostbadge round 2).
 * Inbox tiers and badges use it so rows and counts flip at the same instant.
 */
export function useInboxClockNow(): number {
  return useSyncExternalStore(subscribeInboxClock, getInboxClockNow);
}

/**
 * The 待你处理 count every in-app badge reads — the compact bottom bar
 * (PhoneShell at `/m*`), the desktop sidebar and Shell's compact bar —
 * c-ghostbadge: it is `deriveInboxQueue(...).length`, the SAME projection the
 * `/m/inbox` 待你处理 tier renders. Never re-filter
 * `interactions.filter(state === "pending")` at a call site: that second
 * filter counts ghost cards (a raw-pending durable row whose instance died
 * and whose deadline has elapsed) that the inbox projection excludes.
 *
 * The clock subscription (c-ghostbadge round 2) recomputes when a known
 * deadline crosses even with no store emission, and the timer is re-armed
 * from every interaction page, so the badge flips exactly when the inbox
 * projection does.
 *
 * The OS badge sync outside React (`lib/push.ts`) subscribes to the same
 * clock and calls deriveInboxQueue directly against the store snapshot.
 */
export function useInboxPendingCount(): number {
  const hub = useHub();
  const deviceId = useMemo(() => thisDeviceId(), []);
  const nowMs = useInboxClockNow();
  useEffect(() => {
    syncInboxDeadlineClock(hub.interactions);
  }, [hub.interactions]);
  return useMemo(
    () =>
      deriveInboxQueue({
        interactions: hub.interactions,
        instances: hub.instances,
        hosts: hub.hosts,
        answering: hub.answering,
        deviceId,
        nowMs,
      }).length,
    [hub.interactions, hub.instances, hub.hosts, hub.answering, deviceId, nowMs],
  );
}
