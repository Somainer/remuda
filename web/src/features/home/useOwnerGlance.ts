import { useEffect } from "react";
import { hubStore } from "../../lib/store";

/**
 * home-glance-d060 §5.3: the owner glance is polled ONLY while a home surface
 * (`/m` or `/sessions`) is mounted, and exactly once per cycle.
 *
 * Every home reader (the ① row on both placements, ⑥, the ledger headlines)
 * mounts the same `HomeGlance`, so this hook is called from one place and a
 * module-level refcount keeps ONE interval alive however many surfaces are up
 * (React StrictMode's mount → cleanup → mount included: it decrements to zero
 * and starts again). Mount fetches immediately — a cold bootstrap never shows
 * 「–」 for a whole poll period.
 */

const POLL_MS = 2_000;

let refs = 0;
let timer: ReturnType<typeof setInterval> | null = null;

function start(): void {
  refs += 1;
  if (refs > 1) return;
  // A fetch at mount closes the cold-bootstrap gap (§5.3's first paint).
  void hubStore.refreshGlance();
  timer = setInterval(() => {
    void hubStore.refreshGlance();
  }, POLL_MS);
}

function stop(): void {
  refs = Math.max(0, refs - 1);
  if (refs > 0 || timer === null) return;
  clearInterval(timer);
  timer = null;
}

/** Run the owner-glance poll while the calling component is mounted. */
export function useOwnerGlancePoll(): void {
  useEffect(() => {
    start();
    return stop;
  }, []);
}

/** Test seam: the live refcount (0 = no home surface is polling). */
export function __glancePollRefs(): number {
  return refs;
}

/** Test seam: stop any interval a leaked mount left behind. */
export function __stopGlancePollForTest(): void {
  refs = 0;
  if (timer !== null) {
    clearInterval(timer);
    timer = null;
  }
}
