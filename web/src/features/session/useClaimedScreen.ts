import { useEffect, useState } from "react";
import { api } from "../../lib/api";

/** How often the screen of a claimed pane is re-read while it is on screen. */
export const CLAIMED_SCREEN_POLL_MS = 2000;

/**
 * D-062: a claimed external herdr pane journals no screen snapshots (logs and
 * the journal never carry pane content), so its screen projection reads the
 * live viewport (`GET /v1/instances/{id}/screen`, herdr `pane.read`) while it
 * is shown. `null` id stops polling. A failed read keeps the last lines.
 */
export function useClaimedScreen(instanceId: string | null): string[] | null {
  const [lines, setLines] = useState<string[] | null>(null);
  useEffect(() => {
    if (!instanceId) return;
    let alive = true;
    const read = () => {
      api
        .screenRead(instanceId as Parameters<typeof api.screenRead>[0], 80)
        .then((screen) => {
          if (alive && screen.lines.length) setLines(screen.lines);
        })
        .catch(() => undefined);
    };
    read();
    const timer = setInterval(read, CLAIMED_SCREEN_POLL_MS);
    return () => {
      alive = false;
      clearInterval(timer);
    };
  }, [instanceId]);
  return instanceId ? lines : null;
}
