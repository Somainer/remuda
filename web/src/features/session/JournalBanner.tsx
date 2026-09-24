import css from "./transcript.module.css";

export type JournalUiStatus = "live" | "reconnecting" | "gap-backfill" | "readonly-stale";

export function JournalBanner({
  status,
  onRetry,
}: {
  status: JournalUiStatus;
  onRetry?: () => void;
}) {
  if (status === "live") return null;
  if (status === "reconnecting") {
    return (
      <div className={css.banner} data-testid="journal-banner" data-state="reconnecting">
        重连中 · 仍可输入，发送进入队列，恢复后不会自动重发
      </div>
    );
  }
  if (status === "gap-backfill") {
    return (
      <div className={css.banner} data-testid="journal-banner" data-state="gap-backfill">
        正在补事件 · 工具卡暂不结算
      </div>
    );
  }
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
