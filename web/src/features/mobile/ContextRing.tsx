import css from "./home.module.css";

/**
 * Remaining-context ring for a phone home row (ui-spec §3.3 / E7).
 *
 * `pct` is the Hub rollup's *used* share (0..100). When it is null the ring
 * renders nothing at all — no ring, no "0%", no guessed value — because an
 * unknown share is "不知道怎么画", not zero. Values coming from outside the
 * 0..100 band are clamped rather than trusted, so a bad rollup can never
 * bend the arc backwards.
 */
export function ContextRing({ pct }: { pct: number | null | undefined }) {
  if (pct == null || !Number.isFinite(pct)) return null;
  const used = Math.max(0, Math.min(100, Math.round(pct)));
  const remaining = 100 - used;
  const radius = 8;
  // SVG stroke direction is clockwise from the 3 o'clock point; rotate to
  // start at 12 o'clock and grow the used arc clockwise.
  return (
    <span
      className={css.ring}
      data-testid="context-ring"
      data-pct={used}
      role="img"
      aria-label={`上下文已用 ${used}%，剩余 ${remaining}%`}
      title={`上下文已用 ${used}% · 剩余 ${remaining}%`}
    >
      <svg viewBox="0 0 20 20" width="20" height="20" aria-hidden="true">
        <circle className={css.ringTrack} cx="10" cy="10" r={radius} />
        <circle
          className={css.ringArc}
          cx="10"
          cy="10"
          r={radius}
          pathLength={100}
          strokeDasharray={`${used} 100`}
        />
      </svg>
      <span className={css.ringText} data-testid="context-ring-pct">
        {used}%
      </span>
    </span>
  );
}
