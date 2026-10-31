import { useEffect, useReducer } from "react";
import { nowMs } from "../../lib/clock";
import { hubStore } from "../../lib/store";
import { HOME_FRESHNESS, useHomeFreshness } from "./freshness";
import css from "./hubBanner.module.css";

/**
 * A display clock that only ticks while the banner is relevant. It lives in
 * this component (never in the list) so updating 「N 秒前」 commits the 32px
 * banner alone. The value is read at render time and the tick only schedules
 * re-renders, so becoming visible is exact instead of up to a second late.
 */
function useBannerNow(active: boolean): number {
  const [, tick] = useReducer((count: number) => count + 1, 0);
  useEffect(() => {
    if (!active) return;
    const timer = window.setInterval(tick, 1_000);
    return () => window.clearInterval(timer);
  }, [active]);
  return nowMs();
}

/**
 * §8 横幅: 「连不上 Hub · 上次更新 N 秒前 · 重试」.
 *
 * Shown only after 5 s of CONSECUTIVE failed refreshes (4 s and a recovery
 * never paints it), 32 px high. Any successful refresh clears the failure run
 * in the store, which hides it on the same render. 重试 issues one refresh.
 *
 * An unknown N (no successful snapshot yet) renders 「–」 — never 0 (§0.3).
 */
export function HubBanner() {
  const { lastOkAt, failingSince } = useHomeFreshness();
  const failing = failingSince !== null;
  const now = useBannerNow(failing);
  if (failingSince === null) return null;
  if (now - failingSince < HOME_FRESHNESS.bannerAfterMs) return null;
  const agoLabel =
    lastOkAt === null
      ? "–"
      : `${Math.max(0, Math.floor((now - lastOkAt) / 1_000))} 秒前`;
  // The live region must carry the last-update age too, but at a coarse
  // cadence: minutes, so a screen reader hears one announcement per minute
  // instead of one per countdown tick.
  const announce =
    lastOkAt === null
      ? "连不上 Hub · 上次更新未知"
      : `连不上 Hub · 上次更新 ${Math.max(0, Math.floor((now - lastOkAt) / 60_000))} 分钟前`;
  return (
    <div className={css.banner} data-testid="hub-banner">
      <span className={css.srOnly} role="status">
        {announce}
      </span>
      <span className={css.text} aria-hidden="true">
        连不上 Hub · 上次更新 {agoLabel} ·
      </span>
      <button
        type="button"
        className={css.retry}
        data-testid="hub-banner-retry"
        onClick={() => void hubStore.refresh().catch(() => undefined)}
      >
        重试
      </button>
    </div>
  );
}
