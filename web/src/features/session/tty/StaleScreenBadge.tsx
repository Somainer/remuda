import type { TtyStale } from "./client";
import css from "./TerminalView.module.css";

/**
 * What a reason code means in the header.
 *
 * The Hub's vocabulary, not a free choice: `instance-gone` says the row is
 * terminal (the process is gone, and a Resume is the way back), while
 * `node-link-unavailable` says only that the Hub could not reach the Node —
 * the session may well be alive on the other side of a broken link. Saying the
 * first when only the second is true would tell an operator to give up on a
 * session that is still running, so an unrecognised code falls to the weaker
 * claim.
 */
const REASON_LABEL: Record<string, string> = {
  "instance-gone": "会话已结束",
  "node-link-unavailable": "Node 连接不可用",
};

/** Render an age the way an operator reads it: 12 秒 / 3 分钟 / 2 小时 / 4 天. */
export function formatStaleAge(ageMs: number): string {
  const seconds = Math.max(0, Math.floor(ageMs / 1000));
  if (seconds < 60) return `${seconds} 秒`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes} 分钟`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours} 小时`;
  return `${Math.floor(hours / 24)} 天`;
}

/**
 * The header badge for a screen the browser is showing but that is no longer
 * live.
 *
 * 运行中 alone is the bug this exists to prevent: the instance header reads the
 * row's lifecycle, which says `running` right up until the Hub settles it, so a
 * frozen frame used to sit under a live-looking header with nothing to say the
 * process was gone. The badge always states the age, states the reason when it
 * recognises the code, and never claims liveness. An unrecognised code renders
 * the age alone: a wrong reason is worse than none, because "会话已结束" tells
 * an operator to give up on a session that may still be running.
 */
export function StaleScreenBadge({ stale }: { stale: TtyStale }) {
  const reason = stale.reason ? REASON_LABEL[stale.reason] : undefined;
  const age = stale.ageMs === undefined ? "时间未知" : `${formatStaleAge(stale.ageMs)}前`;
  return (
    <span
      className={css.staleBadge}
      data-testid="tty-stale"
      data-stale-age-ms={stale.ageMs === undefined ? "unknown" : String(stale.ageMs)}
      data-stale-reason={stale.reason ?? "unknown"}
      title="画面来自 Hub 缓存，不是实时输出"
    >
      画面已停更 · {age}
      {reason ? ` · ${reason}` : ""}
    </span>
  );
}
