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
 * Two evidence shapes (ui-spec §2.3): `node-link-unavailable` is freshness
 * UNKNOWN and reads 「画面可能过期」 with a dashed neutral mark; the verified
 * `instance-gone` end state reads 「会话已结束」 in regular frame text, with
 * no dash or warning colour (the Resume entry sits beside it). An
 * unrecognised reason keeps the weaker 「画面已停更」 wording: a wrong reason
 * is worse than none, because "会话已结束" tells an operator to give up on a
 * session that may still be running.
 */
export function StaleScreenBadge({ stale }: { stale: TtyStale }) {
  const age =
    stale.ageMs === undefined ? "时间未知" : `${formatStaleAge(stale.ageMs)}前`;
  let label: string;
  let title: string;
  if (stale.reason === "instance-gone") {
    label = `会话已结束 · ${age}`;
    title = "Hub 已确证进程结束，可从结构化视图继续";
  } else if (stale.reason === "node-link-unavailable") {
    label = `⚠ 画面可能过期 · ${age} · ${REASON_LABEL[stale.reason]}`;
    title = "Hub 暂时连不上 Node；会话在远端可能仍活着，画面新鲜度未知";
  } else {
    label = `画面已停更 · ${age}`;
    title = "画面来自 Hub 缓存，不是实时输出";
  }
  return (
    <span
      className={css.staleBadge}
      data-testid="tty-stale"
      data-stale-age-ms={
        stale.ageMs === undefined ? "unknown" : String(stale.ageMs)
      }
      data-stale-reason={stale.reason ?? "unknown"}
      title={title}
    >
      {label}
    </span>
  );
}
