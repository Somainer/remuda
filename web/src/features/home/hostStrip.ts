import type { Host } from "../../types/instance";
import { hostIsOnline } from "../hosts/model";

/**
 * ② 主机条 (home.md §1 ②, §5.2 tail). Pure derivation for the strip: offline
 * hosts first with how long they have been away, and the collapse to one
 * 「N/M 主机在线」 chip when every host is OK. The component adds only the Hub
 * and notification-permission chips, which come from freshness/push rather
 * than from the host list.
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
};

export type HostStripModel = {
  total: number;
  online: number;
  /** Every host is online: paint one [N/M 主机在线 ›] chip instead of rows. */
  collapsed: boolean;
  /** Offline chips first, then online ones by label. Empty when collapsed. */
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
 * the component offers 「添加主机 ›」 instead.
 */
export function hostStrip(hosts: readonly HostStripHost[], nowMs: number): HostStripModel {
  const online: HostStripChip[] = [];
  const offline: HostStripChip[] = [];
  for (const host of hosts) {
    const chip: HostStripChip = {
      id: host.id,
      label: host.label,
      offline: !hostIsOnline(host),
      age: null,
    };
    if (chip.offline) {
      chip.age = offlineAge(host.lastSeenAt, nowMs);
      offline.push(chip);
    } else {
      online.push(chip);
    }
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
  return {
    total: hosts.length,
    online: online.length,
    collapsed: hosts.length > 0 && offline.length === 0,
    chips: offline.length === 0 ? [] : [...offline, ...online],
  };
}
