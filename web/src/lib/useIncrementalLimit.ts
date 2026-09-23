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
 * Returns `step` on first mount and grows by `step` on each animation frame
 * until it reaches `total`, so a flood (100 pending approvals / inbox rows)
 * commits a few cards per frame instead of hundreds in one task — the
 * scenario-C measurement (docs/design/evidence/inbox-perf-1.md) traced a
 * 2.7 s Long Task to that single commit. Small lists (< `step`) render in one
 * commit exactly as before.
 *
 * Crucially, a plain SHRINK of `total` only clamps the limit: a card leaving
 * the queue (the user answers it, or another device does) must not unmount the
 * rows already revealed, or an in-progress free-text / elicitation draft in a
 * later card would be lost. Slicing restarts only when `resetKey` changes
 * (i.e. the filter itself changed).
 */
export function useIncrementalLimit(
  total: number,
  options: IncrementalLimitOptions = {},
): number {
  const step = options.step ?? 12;
  const resetKey = options.resetKey;
  const [limit, setLimit] = useState(step);
  const [prevResetKey, setPrevResetKey] = useState(resetKey);

  // React's derived-state pattern: only a filter identity change restarts
  // slicing. A grow/shrink of total alone never resets below what is mounted.
  if (!Object.is(resetKey, prevResetKey)) {
    setPrevResetKey(resetKey);
    setLimit(Math.min(step, total));
  }

  // Clamp (never reset) on shrink: answered card drops total by 1, the rest
  // of the revealed rows stay mounted.
  const clamped = Math.min(limit, total);

  useEffect(() => {
    if (clamped >= total) return;
    const frame = requestAnimationFrame(() => setLimit((value) => value + step));
    return () => cancelAnimationFrame(frame);
  }, [clamped, total, step]);

  return clamped;
}
