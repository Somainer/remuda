import { describe, expect, it, vi } from "vitest";
import {
  WakeStore,
  activeWakes,
  formatInterval,
  nextWake,
  orderWakes,
  wakeClock,
  wakeLine,
  wakeNextLabel,
  wakeNotifyLabel,
  wakeTriggerLabel,
  type WakeSource,
  type WakeUp,
} from "./wakes";

/**
 * ⑩'s rules (home.md §6.2, §9): the next-fire line, its 「—」 for a missing
 * time, the sheet's order and labels, and the store's settle-on-Hub-answer
 * cancel (never optimistic) plus its ride on the normal poll.
 */

function wake(patch: Partial<WakeUp> = {}): WakeUp {
  return {
    wakeId: "wak_1",
    lineageId: "lin_1",
    name: "每日汇总",
    text: "汇总",
    trigger: { kind: "every", everySecs: 86400, startAt: undefined },
    mode: "persistent",
    notify: "always",
    state: "active",
    nextFireAt: "2026-11-04T09:00:00Z",
    missedCount: 0,
    createdAt: "2026-11-01T00:00:00Z",
    updatedAt: "2026-11-01T00:00:00Z",
    creator: { origin: "agent" },
    ...patch,
  } as WakeUp;
}

describe("⑩ the Main card's next-fire line (§6.2)", () => {
  it("renders nothing when no wake-up can still fire", () => {
    expect(wakeLine([])).toBeNull();
    expect(wakeLine([wake({ state: "done" })])).toBeNull();
  });

  it("names the earliest next fire and counts the active wake-ups", () => {
    const line = wakeLine([
      wake({ wakeId: "wak_a", name: "早报", nextFireAt: "2026-11-04T10:00:00Z" }),
      wake({ wakeId: "wak_b", name: "午报", nextFireAt: "2026-11-04T09:00:00Z" }),
      wake({ wakeId: "wak_c", name: "已跑过", state: "done", nextFireAt: undefined }),
    ]);
    expect(line).toMatch(/^下次 \d{2}:\d{2} 午报 · 共 2 个$/);
  });

  it("paints 「—」 for a next fire that is null, never 00:00 or the epoch", () => {
    const line = wakeLine([
      wake({ name: "事件提醒", trigger: { kind: "when", factKind: "turn-done" }, nextFireAt: undefined }),
    ]);
    expect(line).toBe("下次 — 事件提醒 · 共 1 个");
    expect(line).not.toContain("00:00");
    expect(wakeNextLabel(wake({ nextFireAt: undefined }))).toBe("—");
  });

  it("formats a clock only for a real timestamp", () => {
    expect(wakeClock("2026-11-04T09:05:00Z")).toMatch(/^\d{2}:\d{2}$/);
    expect(wakeClock(null)).toBeNull();
    expect(wakeClock("not a time")).toBeNull();
  });
});

describe("⑩ labels and order", () => {
  it("names each trigger kind in words", () => {
    expect(wakeTriggerLabel(wake({ trigger: { kind: "at", at: "2026-11-04T09:00:00Z" } }))).toBe("一次");
    expect(wakeTriggerLabel(wake({ trigger: { kind: "every", everySecs: 3600, startAt: undefined } }))).toBe(
      "每 1 小时",
    );
    expect(
      wakeTriggerLabel(wake({ trigger: { kind: "when", factKind: "turn-done" } })),
    ).toBe("事件 · 回合结束");
    expect(formatInterval(86400)).toBe("1 天");
    expect(formatInterval(90)).toBe("90 秒");
  });

  it("names the declared notify policy", () => {
    expect(wakeNotifyLabel("always")).toBe("总是通知");
    expect(wakeNotifyLabel("failure")).toBe("仅失败通知");
    expect(wakeNotifyLabel("never")).toBe("不通知");
  });

  it("orders by the soonest fire, with an unscheduled one last", () => {
    const ordered = orderWakes([
      wake({ wakeId: "wak_when", name: "事件", trigger: { kind: "when", factKind: "gate" }, nextFireAt: undefined }),
      wake({ wakeId: "wak_late", name: "晚", nextFireAt: "2026-11-04T10:00:00Z" }),
      wake({ wakeId: "wak_soon", name: "早", nextFireAt: "2026-11-04T09:00:00Z" }),
    ]);
    expect(ordered.map((row) => row.wakeId)).toEqual(["wak_soon", "wak_late", "wak_when"]);
    expect(activeWakes(ordered)).toHaveLength(3);
    expect(nextWake(ordered)?.wakeId).toBe("wak_soon");
  });
});

type FakeSource = {
  src: WakeSource;
  read: ReturnType<typeof vi.fn>;
  remove: ReturnType<typeof vi.fn>;
  /** Fire one settled-poll notification at `okAt`. */
  poll: (okAt: number) => void;
  setRows: (rows: WakeUp[]) => void;
};

function fakeSource(initial: WakeUp[] = [wake()]): FakeSource {
  let rows = initial;
  let listeners = new Set<() => void>();
  let okAt: number | null = 1;
  const read = vi.fn(async (_lineage: string) => rows);
  const remove = vi.fn(async (_lineage: string, _wakeId: string) => undefined);
  return {
    src: {
      read: read as unknown as WakeSource["read"],
      onRefresh: (listener: () => void) => {
        listeners.add(listener);
        return () => {
          listeners.delete(listener);
        };
      },
      refreshedAt: () => okAt,
      remove: remove as unknown as WakeSource["remove"],
    },
    read,
    remove,
    poll(next: number) {
      okAt = next;
      for (const listener of [...listeners]) listener();
    },
    setRows(next: WakeUp[]) {
      rows = next;
    },
  };
}

describe("⑩ the wake store", () => {
  it("reads once per settled poll and only when the poll succeeded", async () => {
    const fake = fakeSource();
    const store = new WakeStore(fake.src);
    const off = store.subscribe(() => undefined);
    store.setLineage("lin_1");
    await vi.waitFor(() => expect(fake.read).toHaveBeenCalledTimes(1));
    expect(store.getSnapshot()).toMatchObject({ lineageId: "lin_1", loaded: true });
    expect(store.getSnapshot().wakes).toHaveLength(1);
    // A newer successful poll reads again…
    fake.poll(2);
    await vi.waitFor(() => expect(fake.read).toHaveBeenCalledTimes(2));
    // …while the same successful refresh time does not.
    fake.poll(2);
    await Promise.resolve();
    await Promise.resolve();
    expect(fake.read).toHaveBeenCalledTimes(2);
    off();
  });

  it("keeps the last list when a read fails", async () => {
    const fake = fakeSource();
    const store = new WakeStore(fake.src);
    const off = store.subscribe(() => undefined);
    store.setLineage("lin_1");
    await vi.waitFor(() => expect(store.getSnapshot().wakes).toHaveLength(1));
    fake.read.mockImplementation(async () => {
      throw new Error("offline");
    });
    fake.poll(2);
    await vi.waitFor(() => expect(fake.read).toHaveBeenCalledTimes(2));
    expect(store.getSnapshot().wakes).toHaveLength(1);
    off();
  });

  it("cancel waits for the Hub, then drops the row; an in-flight read cannot repaint it", async () => {
    const gate: { release?: (rows: WakeUp[]) => void } = {};
    const fake = fakeSource();
    fake.read.mockImplementation(
      () =>
        new Promise<WakeUp[]>((resolve) => {
          gate.release = resolve;
        }),
    );
    const store = new WakeStore(fake.src);
    const off = store.subscribe(() => undefined);
    store.setLineage("lin_1");
    await vi.waitFor(() => expect(fake.read).toHaveBeenCalledTimes(1));
    // The read is still in flight when the cancel lands.
    await store.cancel("wak_1");
    expect(fake.remove).toHaveBeenCalledWith("lin_1", "wak_1");
    gate.release?.([wake()]);
    await vi.waitFor(() => expect(store.getSnapshot().loaded).toBe(true));
    expect(store.getSnapshot().wakes).toHaveLength(0);
    off();
  });

  it("a failed cancel rejects and leaves the row where it is", async () => {
    const fake = fakeSource();
    fake.remove.mockImplementation(async () => {
      throw new Error("409");
    });
    const store = new WakeStore(fake.src);
    const off = store.subscribe(() => undefined);
    store.setLineage("lin_1");
    await vi.waitFor(() => expect(store.getSnapshot().wakes).toHaveLength(1));
    await expect(store.cancel("wak_1")).rejects.toThrow("409");
    expect(store.getSnapshot().wakes).toHaveLength(1);
    off();
  });

  it("a chapter switch never paints the previous lineage's rows", async () => {
    const fake = fakeSource();
    const store = new WakeStore(fake.src);
    const off = store.subscribe(() => undefined);
    store.setLineage("lin_1");
    await vi.waitFor(() => expect(store.getSnapshot().wakes).toHaveLength(1));
    store.setLineage("lin_2");
    expect(store.getSnapshot()).toEqual({ lineageId: "lin_2", wakes: [], loaded: false });
    off();
  });
});
