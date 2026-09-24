import { useMemo } from "react";
import { useHub } from "../../lib/store";
import { thisDeviceId } from "../../lib/interactionStatus";
import { deriveInboxQueue } from "./inboxRows";

/**
 * The 待你处理 count every in-app badge reads — the compact bottom bar
 * (PhoneShell at `/m*`), the desktop sidebar and Shell's compact bar —
 * c-ghostbadge: it is `deriveInboxQueue(...).length`, the SAME projection the
 * `/m/inbox` 待你处理 tier renders. Never re-filter
 * `interactions.filter(state === "pending")` at a call site: that second
 * filter counts ghost cards (a raw-pending durable row whose instance died
 * and whose deadline has elapsed) that the inbox projection excludes.
 *
 * The OS badge sync outside React (`lib/push.ts`) calls deriveInboxQueue
 * directly against the store snapshot.
 */
export function useInboxPendingCount(): number {
  const hub = useHub();
  const deviceId = useMemo(() => thisDeviceId(), []);
  return useMemo(
    () =>
      deriveInboxQueue({
        interactions: hub.interactions,
        instances: hub.instances,
        hosts: hub.hosts,
        answering: hub.answering,
        deviceId,
      }).length,
    [hub.interactions, hub.instances, hub.hosts, hub.answering, deviceId],
  );
}
