import { useCallback, useEffect, useSyncExternalStore } from "react";
import { rest } from "../../lib/api";
import type { components } from "../../lib/api.generated";
import { hubStore } from "../../lib/store";

/**
 * ⑩ 定时 / 今天 (home.md §1 ⑩, §6.2, §9; task home-wakes, D-060
 * cb-wake-engine).
 *
 * Main's lineage owns its wake-ups. The list is read with
 * `GET /v1/lineages/{id}/wake-ups` and one is removed with
 * `DELETE /v1/lineages/{id}/wake-ups/{wake}` — both Human-only on the Hub,
 * which is exactly the surface that mounts this store (an Agent never sees the
 * home). The store rides the Hub's settled list poll (home.md §8: "refreshes on
 * the normal poll"), exactly like the ④ notices store, so a wake-up Main
 * adds or someone cancels lands within one poll period.
 *
 * Every number and time here is real: the line names the earliest ACTIVE
 * wake-up's next fire, 「共 N 个」 counts the active ones, and a missing next
 * fire (a `when` trigger, or one a cancelled add left without one) paints
 * 「—」, never 00:00 or the epoch.
 */

export type WakeUp = components["schemas"]["WakeUp"];
export type WakeNotify = WakeUp["notify"];

/** A lineage's wake-ups (the list route answers `WakeList`). */
export async function listWakes(lineageId: string, signal?: AbortSignal): Promise<WakeUp[]> {
  const page = await rest<{ items?: WakeUp[] }>(wakePath(lineageId), signal ? { signal } : {});
  return Array.isArray(page?.items) ? page.items : [];
}

/** Remove one wake-up; its runs stay as history on the Hub. */
export async function removeWake(lineageId: string, wakeId: string): Promise<void> {
  await rest(`${wakePath(lineageId)}/${encodeURIComponent(wakeId)}`, { method: "DELETE" });
}

export function wakePath(lineageId: string): string {
  return `/v1/lineages/${encodeURIComponent(lineageId)}/wake-ups`;
}

/** The wake-ups that will still fire (a fired `at` row is `done`). */
export function activeWakes(wakes: readonly WakeUp[]): WakeUp[] {
  return wakes.filter((wake) => wake.state === "active");
}

/** The earliest next fire; ties by name so the line never flickers. */
export function nextWake(wakes: readonly WakeUp[]): WakeUp | null {
  let best: WakeUp | null = null;
  let bestAt = Number.POSITIVE_INFINITY;
  for (const wake of activeWakes(wakes)) {
    const at = wake.nextFireAt ? Date.parse(wake.nextFireAt) : Number.NaN;
    if (!Number.isFinite(at)) continue;
    if (at < bestAt || (at === bestAt && best !== null && wake.name < best.name)) {
      best = wake;
      bestAt = at;
    }
  }
  return best;
}

/** 「HH:MM」 on the local clock; null when there is no scheduled occurrence. */
export function wakeClock(iso: string | null | undefined): string | null {
  if (!iso) return null;
  const at = Date.parse(iso);
  if (!Number.isFinite(at)) return null;
  return new Date(at).toLocaleTimeString([], {
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  });
}

/**
 * The Main card's optional third line (home.md §6.2): 「下次 HH:MM <name> ·
 * 共 N 个」, or null when no wake-up can still fire (then the line is not
 * rendered at all). A next fire that is null reads 「—」.
 */
export function wakeLine(wakes: readonly WakeUp[]): string | null {
  const active = activeWakes(wakes);
  if (active.length === 0) return null;
  const next = nextWake(wakes) ?? active[0];
  const clock = wakeClock(next.nextFireAt) ?? "—";
  return `下次 ${clock} ${next.name} · 共 ${active.length} 个`;
}

/** One scheduled occurrence, for the sheet row's 下次 column. */
export function wakeNextLabel(wake: WakeUp): string {
  return wakeClock(wake.nextFireAt) ?? "—";
}

/** A human interval for an `every` trigger. */
export function formatInterval(secs: number): string {
  if (secs % 86400 === 0) return `${secs / 86400} 天`;
  if (secs % 3600 === 0) return `${secs / 3600} 小时`;
  if (secs % 60 === 0) return `${secs / 60} 分`;
  return `${secs} 秒`;
}

const FACT_KIND_LABEL: Record<string, string> = {
  started: "会话启动",
  exited: "会话退出",
  waiting: "会话等待",
  "turn-done": "回合结束",
  "turn-failed": "回合失败",
  interaction: "待处理交互",
  "interaction-superseded": "交互离队",
  gate: "落地闸门",
  "supply-reset": "额度重置",
  "host-online": "主机上线",
  "host-offline": "主机离线",
  stalled: "任务停滞",
  "budget-expired": "预算用尽",
  "wake-due": "定时触发",
  "context-threshold": "上下文阈值",
};

/** What fires the wake-up, in the sheet's own words. */
export function wakeTriggerLabel(wake: WakeUp): string {
  const trigger = wake.trigger;
  if (trigger.kind === "at") return "一次";
  if (trigger.kind === "every") return `每 ${formatInterval(trigger.everySecs)}`;
  const label = FACT_KIND_LABEL[trigger.factKind] ?? trigger.factKind;
  return `事件 · ${label}`;
}

/** Which runs notify the owner, declared by the caller (never the Hub's choice). */
export function wakeNotifyLabel(notify: WakeNotify): string {
  if (notify === "always") return "总是通知";
  if (notify === "failure") return "仅失败通知";
  return "不通知";
}

/** The sheet's order: soonest fire first, `when` (no clock) last, then by name. */
export function orderWakes(wakes: readonly WakeUp[]): WakeUp[] {
  return [...wakes].sort((a, b) => {
    const ta = a.state === "active" && a.nextFireAt ? Date.parse(a.nextFireAt) : Number.POSITIVE_INFINITY;
    const tb = b.state === "active" && b.nextFireAt ? Date.parse(b.nextFireAt) : Number.POSITIVE_INFINITY;
    if (ta !== tb) return ta < tb ? -1 : 1;
    return a.name < b.name ? -1 : a.name > b.name ? 1 : a.wakeId < b.wakeId ? -1 : 1;
  });
}

export type WakeSnapshot = {
  /** The lineage this snapshot belongs to; null before the first request. */
  lineageId: string | null;
  /** Its wake-ups, oldest first (the route's order). */
  wakes: readonly WakeUp[];
  /** A read has answered for the current lineage. */
  loaded: boolean;
};

/** What the store reads with; injectable so tests never touch the network. */
export type WakeSource = {
  read: (lineageId: string, signal?: AbortSignal) => Promise<WakeUp[]>;
  /** Subscribe to the moments the list should be read again (settled polls). */
  onRefresh: (listener: () => void) => () => void;
  refreshedAt: () => number | null;
  remove: (lineageId: string, wakeId: string) => Promise<void>;
};

const HUB_SOURCE: WakeSource = {
  read: listWakes,
  onRefresh: (listener) => hubStore.subscribeRefreshHealth(listener),
  refreshedAt: () => hubStore.getRefreshHealth().lastOkAt,
  remove: removeWake,
};

function sameWakes(a: readonly WakeUp[], b: readonly WakeUp[]): boolean {
  if (a.length !== b.length) return false;
  return a.every(
    (wake, index) =>
      wake.wakeId === b[index].wakeId
      && wake.name === b[index].name
      && wake.state === b[index].state
      && wake.nextFireAt === b[index].nextFireAt
      && wake.missedCount === b[index].missedCount
      && wake.notify === b[index].notify
      && wake.updatedAt === b[index].updatedAt,
  );
}

/**
 * The wake-ups of one lineage (Main's), shared by the Main card's line and
 * the 「今天」 card. One read at a time, one read per settled list refresh, and
 * a snapshot whose identity changes only when the list does.
 */
export class WakeStore {
  private snapshot: WakeSnapshot = { lineageId: null, wakes: [], loaded: false };
  private listeners = new Set<() => void>();
  private inflight: Promise<void> | null = null;
  private again = false;
  private stop: (() => void) | null = null;
  private lastOkAt: number | null = null;
  /** Wake-ups cancelled from this page; a read in flight must not repaint one. */
  private cancelled = new Set<string>();
  private lineageId: string | null = null;

  private readonly source: WakeSource;

  constructor(source: WakeSource = HUB_SOURCE) {
    this.source = source;
  }

  getSnapshot = (): WakeSnapshot => this.snapshot;

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    if (this.listeners.size === 1) this.start();
    return () => {
      this.listeners.delete(listener);
      if (this.listeners.size === 0) {
        this.stop?.();
        this.stop = null;
      }
    };
  };

  /**
   * Point the store at the resolved Main lineage. A chapter switch keeps the
   * same lineage; a seat change resets the snapshot so the previous lineage's
   * rows are never painted under the new one.
   */
  setLineage(lineageId: string | null): void {
    if (this.lineageId === lineageId) return;
    this.lineageId = lineageId;
    this.cancelled = new Set();
    this.emit({ lineageId, wakes: [], loaded: false });
    if (lineageId) void this.refresh();
  }

  private start() {
    this.stop = this.source.onRefresh(() => {
      // Only a SUCCESSFUL list refresh reads again: a failing Hub is not
      // asked twice per failure.
      const at = this.source.refreshedAt();
      if (at === null || at === this.lastOkAt) return;
      this.lastOkAt = at;
      void this.refresh();
    });
    this.lastOkAt = this.source.refreshedAt();
    void this.refresh();
  }

  private emit(next: WakeSnapshot) {
    this.snapshot = next;
    for (const listener of [...this.listeners]) listener();
  }

  /** Read the list now (joined with a read already in flight). */
  refresh(): Promise<void> {
    if (!this.lineageId) return Promise.resolve();
    if (this.inflight) {
      this.again = true;
      return this.inflight;
    }
    const run = (async () => {
      try {
        do {
          this.again = false;
          const lineageId = this.lineageId;
          if (!lineageId) return;
          try {
            const wakes = (await this.source.read(lineageId)).filter((wake) => !this.cancelled.has(wake.wakeId));
            if (this.lineageId !== lineageId) continue;
            if (!this.snapshot.loaded || this.snapshot.lineageId !== lineageId || !sameWakes(wakes, this.snapshot.wakes)) {
              this.emit({ lineageId, wakes, loaded: true });
            }
          } catch {
            // Quiet: the next settled refresh reads again; the surfaces keep
            // what they have (home.md §8 greys them when the Hub is gone).
          }
        } while (this.again);
      } finally {
        this.inflight = null;
      }
    })();
    this.inflight = run;
    return run;
  }

  /**
   * Cancel one wake-up on the Hub, then drop it from every surface. A failure
   * rejects and leaves the row where it is (the operator can retry).
   */
  async cancel(wakeId: string): Promise<void> {
    const lineageId = this.lineageId;
    if (!lineageId) return;
    await this.source.remove(lineageId, wakeId);
    this.cancelled.add(wakeId);
    const wakes = this.snapshot.wakes.filter((wake) => wake.wakeId !== wakeId);
    if (wakes.length !== this.snapshot.wakes.length) this.emit({ ...this.snapshot, wakes });
  }
}

export const wakeStore = new WakeStore();

/**
 * Main's wake-ups. `lineageId` is the resolved Main lineage; the store is
 * pointed at it, and a snapshot for any other lineage reads as not loaded so
 * a chapter switch never paints the old seat's rows.
 */
export function useMainWakes(lineageId: string | null, store: WakeStore = wakeStore): WakeSnapshot {
  // The subscriber keeps the store alive for exactly this surface; the effect
  // points it at the current lineage (a chapter switch keeps the same one).
  const subscribe = useCallback((listener: () => void) => store.subscribe(listener), [store]);
  useEffect(() => {
    store.setLineage(lineageId);
  }, [store, lineageId]);
  const snapshot = useSyncExternalStore(subscribe, store.getSnapshot, store.getSnapshot);
  if (snapshot.lineageId !== lineageId) return { lineageId, wakes: [], loaded: false };
  return snapshot;
}
