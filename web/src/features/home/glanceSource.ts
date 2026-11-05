import { useEffect, useMemo } from "react";
import { thisDeviceId } from "../../lib/interactionStatus";
import { useHub } from "../../lib/store";
import { syncInboxDeadlineClock } from "../mobile/inboxClock";
import { useInboxClockNow } from "../mobile/useInboxPendingCount";
import { useTaskLedger } from "../tasks/taskLedger";
import type { GlanceSource } from "./glance";

/**
 * The ONE client page §5.1 N is derived from (`glanceCounts().needYou`): the
 * store's instances, interactions and hosts, the owner-wide task ledger and the
 * inbox deadline clock. ④'s header and ①'s 需要你 both read it, so the two
 * numbers are known at the same moment and always agree (home.md §5.1, §5.3).
 * Owner-wide: a `?project=` filter narrows the ledger list, never N.
 */
export function useGlanceSource(): GlanceSource {
  const hub = useHub();
  const { tasks } = useTaskLedger(null);
  const deviceId = useMemo(() => thisDeviceId(), []);
  const nowMs = useInboxClockNow();
  useEffect(() => {
    syncInboxDeadlineClock(hub.interactions);
  }, [hub.interactions]);
  return useMemo<GlanceSource>(
    () => ({
      instances: hub.instances,
      interactions: hub.interactions,
      hosts: hub.hosts,
      tasks,
      answering: hub.answering,
      deviceId,
      nowMs,
    }),
    [hub.instances, hub.interactions, hub.hosts, tasks, hub.answering, deviceId, nowMs],
  );
}
