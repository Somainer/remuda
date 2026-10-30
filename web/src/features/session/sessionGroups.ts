import type { HomeRank } from "../mobile/homeRows";

/**
 * The desktop status buckets, in paint order. Every `HomeRank` belongs to
 * exactly one bucket (a test walks HOME_RANKS): a rank with no bucket would
 * silently drop its session from /sessions.
 */
export const SESSION_GROUPS: { id: string; title: string; match: (rank: HomeRank) => boolean }[] = [
  { id: "blocked", title: "待处理", match: (rank) => rank === "blocked" },
  // seen-hub: a settled turn the owner has not opened yet.
  { id: "unseen", title: "完成未看", match: (rank) => rank === "done-unseen" },
  { id: "working", title: "进行中", match: (rank) => rank === "working" || rank === "awaiting-wake" || rank === "starting" },
  { id: "recent", title: "最近", match: (rank) => rank === "idle" },
  // D-028 §8 方案 A: node-epoch-changed settles the row as exited; it leaves
  // the live groups and lands here, next to its Resume button.
  { id: "exited", title: "已退出", match: (rank) => rank === "exited" },
];

