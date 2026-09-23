import { useEffect, useState } from "react";

/**
 * Progressive-mount limit for long lists.
 *
 * Returns `step` on first render and grows by `step` on each animation frame
 * until it reaches `total`. A flood (100 pending approvals / inbox rows)
 * therefore commits a few cards per frame instead of hundreds in one task —
 * the scenario-C measurement (docs/design/evidence/inbox-perf-1.md) traced a
 * 2.7 s Long Task to that single commit. Every row still mounts within a
 * handful of frames, so counts, filters and deep links keep seeing the whole
 * list; small lists (< `step`) render in one commit exactly as before.
 *
 * When `total` shrinks (e.g. a kind filter narrows the list) the limit
 * restarts, so when it later grows back (filter cleared) the list again
 * mounts in slices rather than as one big commit. Plain growth overshoot
 * (20 + 12 past 25) is just clamped, never treated as a shrink.
 */
export function useIncrementalLimit(total: number, step = 12): number {
  const [limit, setLimit] = useState(step);
  const [prevTotal, setPrevTotal] = useState(total);

  // Adjust state during render (React's derived-state pattern), only on a
  // real shrink — growth keeps walking the limit up via the rAF effect.
  if (total < prevTotal) {
    setPrevTotal(total);
    setLimit(Math.min(step, total));
  } else if (total > prevTotal) {
    setPrevTotal(total);
  }

  useEffect(() => {
    if (limit >= total) return;
    const frame = requestAnimationFrame(() => setLimit((value) => value + step));
    return () => cancelAnimationFrame(frame);
  }, [limit, total, step]);

  return Math.min(limit, total);
}
