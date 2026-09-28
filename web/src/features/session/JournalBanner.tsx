import { useEffect, useRef, useState } from "react";
import { useHub } from "../../lib/store";
import css from "./transcript.module.css";

export type JournalUiStatus = "live" | "reconnecting" | "recovering" | "stale" | "gap-backfill" | "readonly-stale";

type BannerState = JournalUiStatus | "offline" | "restored";

/**
 * Event-completeness (gap-backfill / readonly-stale) comes from the per
 * journal; reachability (offline / recovering / stale) comes from the D-055
 * connection machine. Completeness problems outrank the link label (they need
 * the 重试 action); a quiet stale link shows no banner at all.
 */
function effectiveStatus(journalStatus: JournalUiStatus, connection: string): BannerState {
  if (journalStatus === "readonly-stale" || journalStatus === "gap-backfill") return journalStatus;
  if (connection === "offline") return "offline";
  if (connection === "recovering") return "recovering";
  return journalStatus;
}

export function JournalBanner({
  status,
  onRetry,
}: {
  status: JournalUiStatus;
  onRetry?: () => void;
}) {
  const connection = useHub().connection;
  const pendingCount = useHub().outboxPending;
  const shown = effectiveStatus(status, connection);

  // Brief 「已恢复」 notice when the link returns live after an OFFLINE spell
  // (offline → recovering → live), ~1.5 s (D-055 §3.3 UI copy). A mere
  // stale→live re-certification must NOT (re)arm it: a quiet session whose
  // only frame is its open snapshot flaps through stale every frame-watchdog
  // window, and re-arming the notice on each flap left it stuck forever.
  // `sawOffline` latches an actual offline state since the last live spell
  // (a stale watchdog flap does not latch); the timer clears the notice, and
  // the render gate (`shown === "live"`) hides it the instant the link drops.
  // Leaving live within the window (a new mount goes recovering) also clears
  // `restored` in the cleanup: the timer is gone, so leaving the notice set
  // would let it latch permanently when the link returns live.
  const [restored, setRestored] = useState(false);
  const sawOffline = useRef(false);
  useEffect(() => {
    if (connection === "offline") {
      sawOffline.current = true;
      return;
    }
    if (connection !== "live" || !sawOffline.current) return;
    sawOffline.current = false;
    setRestored(true);
    const t = setTimeout(() => setRestored(false), 1500);
    return () => {
      clearTimeout(t);
      setRestored(false);
    };
  }, [connection]);

  if (restored && shown === "live") {
    return (
      <div className={css.banner} data-testid="journal-banner" data-state="restored">
        已恢复
      </div>
    );
  }
  if (shown === "live" || shown === "stale") return null;
  if (shown === "reconnecting" || shown === "recovering") {
    return (
      <div className={css.banner} data-testid="journal-banner" data-state="recovering">
        正在恢复…
      </div>
    );
  }
  if (shown === "gap-backfill") {
    return (
      <div className={css.banner} data-testid="journal-banner" data-state="gap-backfill">
        正在补事件 · 工具卡暂不结算
      </div>
    );
  }
  if (shown === "readonly-stale") {
    return (
      <div className={css.banner} data-testid="journal-banner" data-state="readonly-stale">
        只读 · 事件可能不完整
        {onRetry ? (
          <button type="button" className={css.bannerRetry} onClick={onRetry}>
            重试
          </button>
        ) : null}
      </div>
    );
  }
  // offline: inputs still work; the outbox delivers on recovery.
  return (
    <div className={css.banner} data-testid="journal-banner" data-state="offline">
      离线 · 可继续输入，恢复后自动发送{pendingCount > 0 ? `（${pendingCount} 条待发）` : ""}
    </div>
  );
}
