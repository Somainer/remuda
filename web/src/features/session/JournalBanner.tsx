import { useEffect, useRef, useState } from "react";
import { useHub } from "../../lib/store";
import ui from "../../styles/ui.module.css";

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

  // Brief 「已恢复」 notice when the link returns live after a non-live spell
  // (offline/recovering/stale), ~1.5 s (D-055 §3.3 UI copy).
  const [restored, setRestored] = useState(false);
  const prev = useRef(connection);
  useEffect(() => {
    const wasDown = prev.current === "offline" || prev.current === "recovering" || prev.current === "stale";
    if (wasDown && connection === "live") {
      setRestored(true);
      const t = setTimeout(() => setRestored(false), 1500);
      prev.current = connection;
      return () => clearTimeout(t);
    }
    prev.current = connection;
  }, [connection]);

  if (restored && shown === "live") {
    return (
      <div className={ui.card} data-testid="journal-banner" data-state="restored" style={{ margin: "8px 12px 0" }}>
        已恢复
      </div>
    );
  }
  if (shown === "live" || shown === "stale") return null;
  if (shown === "reconnecting" || shown === "recovering") {
    return (
      <div className={ui.card} data-testid="journal-banner" data-state="recovering" style={{ margin: "8px 12px 0" }}>
        正在恢复…
      </div>
    );
  }
  if (shown === "gap-backfill") {
    return (
      <div className={ui.card} data-testid="journal-banner" data-state="gap-backfill" style={{ margin: "8px 12px 0" }}>
        正在补事件 · 工具卡暂不结算
      </div>
    );
  }
  if (shown === "readonly-stale") {
    return (
      <div className={ui.card} data-testid="journal-banner" data-state="readonly-stale" style={{ margin: "8px 12px 0" }}>
        只读 · 事件可能不完整
        {onRetry ? (
          <button type="button" className={ui.chip} style={{ marginLeft: 8 }} onClick={onRetry}>
            重试
          </button>
        ) : null}
      </div>
    );
  }
  // offline: inputs still work; the outbox delivers on recovery.
  return (
    <div className={ui.card} data-testid="journal-banner" data-state="offline" style={{ margin: "8px 12px 0" }}>
      离线 · 可继续输入，恢复后自动发送{pendingCount > 0 ? `（${pendingCount} 条待发）` : ""}
    </div>
  );
}
