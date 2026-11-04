import type { Host } from "../../types/instance";
import { hostIsOnline } from "../hosts/model";

/**
 * ② 主机条 (home.md §1 ②, §5.2 tail). Pure derivation for the strip: offline
 * hosts first with how long they have been away, and the collapse to one
 * 「N/M 主机在线」 chip when every host is OK — keeping Main's own host chip
 * visible beside that summary so ★ is never hidden (§1 ②). The component adds
 * only the Hub and notification-permission chips, which come from
 * freshness/push rather than from the host list.
 */

export type HostStripHost = Pick<Host, "id" | "label" | "state" | "lastSeenAt"> & {
  online?: boolean;
};

export type HostStripChip = {
  id: string;
  label: string;
  offline: boolean;
  /** How long ago this host was last seen; null when it was never seen. */
  age: string | null;
  /** Main's seat runs here (home.md §1 ②): the chip paints ★. */
  main?: boolean;
};

export type HostStripModel = {
  total: number;
  online: number;
  /** Every host is online: paint the [N/M 主机在线 ›] summary chip too. */
  collapsed: boolean;
  /**
   * Offline chips first, then online ones by label. When collapsed this holds
   * Main's host chip alone (so ★ stays visible), or nothing with no Main.
   */
  chips: HostStripChip[];
};

/** 「2h」 / 「15m」 / 「3d」 / 「刚刚」 for an offline host's last heartbeat. */
export function offlineAge(lastSeenAt: string | undefined, nowMs: number): string | null {
  if (!lastSeenAt) return null;
  const at = Date.parse(lastSeenAt);
  if (!Number.isFinite(at)) return null;
  const delta = Math.max(0, nowMs - at);
  if (delta < 60_000) return "刚刚";
  if (delta < 3_600_000) return `${Math.floor(delta / 60_000)}m`;
  if (delta < 86_400_000) return `${Math.floor(delta / 3_600_000)}h`;
  return `${Math.floor(delta / 86_400_000)}d`;
}

function lastSeenMs(host: HostStripHost): number {
  const at = host.lastSeenAt ? Date.parse(host.lastSeenAt) : Number.NaN;
  return Number.isFinite(at) ? at : Number.POSITIVE_INFINITY;
}

/**
 * Ordering: offline first (longest away first, since that is the one worth
 * noticing), then online hosts by label. An empty host list is not collapsed:
 * the component offers 「添加主机 ›」 instead. `mainHostId` marks the host Main
 * runs on (★, home.md §1 ②, task home-main-extras); it is absent for a host
 * list with no Main.
 *
 * When every host is online the strip collapses to one summary chip, but
 * Main's host keeps its own chip beside that summary: the wireframe draws the
 * star on the host chip (`forge★`), and the owner still needs to see where
 * Main sits in the normal, all-healthy state. With no Main the collapsed
 * strip is the summary alone.
 */
export function hostStrip(
  hosts: readonly HostStripHost[],
  nowMs: number,
  mainHostId: string | null = null,
): HostStripModel {
  const online: HostStripChip[] = [];
  const offline: HostStripChip[] = [];
  let mainChip: HostStripChip | null = null;
  for (const host of hosts) {
    const chip: HostStripChip = {
      id: host.id,
      label: host.label,
      offline: !hostIsOnline(host),
      age: null,
      main: mainHostId !== null && host.id === mainHostId,
    };
    if (chip.offline) {
      chip.age = offlineAge(host.lastSeenAt, nowMs);
      offline.push(chip);
    } else {
      online.push(chip);
    }
    if (chip.main) mainChip = chip;
  }
  const byLabel = (a: HostStripChip, b: HostStripChip) =>
    a.label.localeCompare(b.label) || a.id.localeCompare(b.id);
  const seenAt = new Map(hosts.map((host) => [host.id, lastSeenMs(host)] as const));
  offline.sort(
    (a, b) =>
      seenAt.get(a.id)! - seenAt.get(b.id)! ||
      byLabel(a, b),
  );
  online.sort(byLabel);
  const collapsed = hosts.length > 0 && offline.length === 0;
  return {
    total: hosts.length,
    online: online.length,
    collapsed,
    chips: collapsed ? (mainChip ? [mainChip] : []) : [...offline, ...online],
  };
}
