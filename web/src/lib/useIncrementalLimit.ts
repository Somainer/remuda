import { useEffect, useState } from "react";

export type IncrementalLimitOptions = {
  /** Rows revealed per animation frame. */
  step?: number;
  /**
   * Identity of the filter/view the list is currently showing. When this key
   * changes (kind segment, host/workspace filter), progressive mount restarts
   * from the first slice — the list now shows a different set. Key it on the
   * filter itself.
   */
  resetKey?: unknown;
};

/**
 * Progressive-mount limit for long lists.
 *
 * Starts at `step` and grows by `step` on each animation frame until it
 * reaches `total`, so a flood (100 pending approvals / inbox rows) commits a
 * few cards per frame instead of hundreds in one task — the scenario-C
 * measurement (docs/design/evidence/inbox-perf-1.md) traced a 2.7 s Long Task
 * to that single commit. Small lists (< `step`) render in one commit exactly
 * as before.
 *
 * Two distinct ways the count changes, handled differently:
 *  - SHRINK with the same filter (a card is answered locally or on another
 *    device): persist `limit = min(limit, total)`. The revealed tail that
 *    remains stays mounted (an open free-text / elicitation draft is never
 *    lost), AND the stored high-water mark is lowered — so when the list later
 *    grows again (new interactions arrive with no filter change) the new tail
 *    mounts in `step` slices, not as one jump back to the old high-water mark.
 *  - FILTER change (`resetKey`): a genuinely different set, so restart from
 *    the first slice.
 */
export function useIncrementalLimit(
  total: number,
  options: IncrementalLimitOptions = {},
): number {
  const step = options.step ?? 12;
  const resetKey = options.resetKey;
  const [limit, setLimit] = useState(step);
  const [prevResetKey, setPrevResetKey] = useState(resetKey);

  // React's derived-state-during-render pattern. A filter identity change
  // restarts slicing; otherwise a shrink lowers the stored limit to exactly
  // what is mounted (never reset to step, never left at a stale high mark).
  if (!Object.is(resetKey, prevResetKey)) {
    setPrevResetKey(resetKey);
    setLimit(Math.min(step, total));
  } else if (limit > total) {
    setLimit(total);
  }

  useEffect(() => {
    if (limit >= total) return;
    const frame = requestAnimationFrame(() => setLimit((value) => value + step));
    return () => cancelAnimationFrame(frame);
  }, [limit, total, step]);

  return Math.min(limit, total);
}
