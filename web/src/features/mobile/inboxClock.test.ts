import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Interaction } from "../../types/interaction";
import { known } from "../../types/wire";
import {
  __resetInboxClockForTest,
  getInboxClockNow,
  subscribeInboxClock,
  syncInboxDeadlineClock,
} from "./inboxClock";

const T0 = Date.parse("2026-09-20T10:00:00.000Z");

function card(deadlineISO: string, id = "int_1"): Interaction {
  return {
    id,
    state: "pending",
    deadline: known(deadlineISO),
  } as unknown as Interaction;
}

describe("inboxClock (c-ghostbadge round 2)", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(T0);
    __resetInboxClockForTest(T0);
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("ticks once when the soonest known deadline crosses", () => {
    const ticks: number[] = [];
    const off = subscribeInboxClock(() => ticks.push(getInboxClockNow()));
    syncInboxDeadlineClock([card(new Date(T0 + 100).toISOString())], T0);

    vi.advanceTimersByTime(102);
    expect(ticks).toHaveLength(1);
    expect(ticks[0]!).toBeGreaterThanOrEqual(T0 + 101);
    off();
  });

  it("periodic store re-syncs notify on every clock advance and never postpone the deadline tick", () => {
    // Reproduces the e2e failure: a 2 s poll re-enters the sync before
    // the deadline. It must neither freeze clockNow nor reschedule the tick past
    // the crossing, and (round 3) every snapshot advance must reach subscribers.
    const ticks: number[] = [];
    const off = subscribeInboxClock(() => ticks.push(getInboxClockNow()));
    syncInboxDeadlineClock([card(new Date(T0 + 100).toISOString())], T0);
    expect(ticks).toHaveLength(0);

    // A poll lands at +50 ms (fresh page, same card, explicit wall time):
    // the clock advances and subscribers are notified immediately.
    vi.advanceTimersByTime(50);
    syncInboxDeadlineClock([card(new Date(T0 + 100).toISOString())], T0 + 50);
    expect(getInboxClockNow()).toBe(T0 + 50);
    expect(ticks).toEqual([T0 + 50]);

    // Another poll at +90 ms.
    vi.advanceTimersByTime(40);
    syncInboxDeadlineClock([card(new Date(T0 + 100).toISOString())], T0 + 90);
    expect(ticks).toEqual([T0 + 50, T0 + 90]);

    // Crossing: exactly one further tick, armed from the +90 re-sync at the
    // deadline + slack — no later.
    vi.advanceTimersByTime(15);
    expect(ticks).toHaveLength(3);
    expect(ticks[2]!).toBeGreaterThanOrEqual(T0 + 100);
    expect(ticks[2]!).toBeLessThanOrEqual(T0 + 102);
    off();
  });

  it("notifies both flips when a resync crosses a deadline before its timer fires (round 3)", () => {
    // Two deadlines 1 s apart. The timer for the first has +2 ms slack; a
    // resync landing in that gap (past the deadline, before the tick) used to
    // cancel the timer and silently advance the clock — the first flip was lost
    // forever. Every snapshot advance must emit, and the timer re-arms for the
    // next deadline, so both flips reach subscribers.
    const ticks: number[] = [];
    const off = subscribeInboxClock(() => ticks.push(getInboxClockNow()));
    const cards = [
      card(new Date(T0 + 1000).toISOString(), "int_1"),
      card(new Date(T0 + 2000).toISOString(), "int_2"),
    ];
    syncInboxDeadlineClock(cards, T0);

    // +1001 ms: deadline 1 has crossed (at +1000) but its timer (slack
    // +2 ms) has not fired yet. A store page arrives and re-syncs.
    vi.advanceTimersByTime(1001);
    syncInboxDeadlineClock(cards, T0 + 1001);

    // The crossed deadline flips via the resync emit itself.
    expect(ticks).toHaveLength(1);
    expect(ticks[0]!).toBeGreaterThanOrEqual(T0 + 1000);
    expect(ticks[0]!).toBeLessThan(T0 + 2000);

    // Deadline 2 still fires on its own timer.
    vi.advanceTimersByTime(1002);
    expect(ticks).toHaveLength(2);
    expect(ticks[1]!).toBeGreaterThanOrEqual(T0 + 2000);
    off();
  });

  it("arms no timer when the deadline is already past at sync time", () => {
    const ticks: number[] = [];
    const off = subscribeInboxClock(() => ticks.push(getInboxClockNow()));
    syncInboxDeadlineClock([card(new Date(T0 - 1).toISOString())], T0);
    vi.advanceTimersByTime(1000);
    expect(ticks).toHaveLength(0);
    off();
  });

  it("ignores non-pending rows and unknown deadlines", () => {
    const ticks: number[] = [];
    const off = subscribeInboxClock(() => ticks.push(getInboxClockNow()));
    syncInboxDeadlineClock(
      [{ id: "x", state: "expired", deadline: known(new Date(T0 + 1).toISOString()) }] as never,
      T0,
    );
    vi.advanceTimersByTime(1000);
    expect(ticks).toHaveLength(0);
    off();
  });
});

describe("inboxClock independent display tick (c-perffu r3)", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(T0);
    __resetInboxClockForTest(T0);
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("advances on its own with equal payloads and no deadlines at all", () => {
    const ticks: number[] = [];
    const off = subscribeInboxClock(() => ticks.push(getInboxClockNow()));

    // No syncInboxDeadlineClock call: no pending deadline exists. Repeated
    // equal data pages would be no-emit polls, yet labels must still age.
    vi.advanceTimersByTime(46_000);
    expect(ticks.length).toBeGreaterThanOrEqual(3);
    expect(getInboxClockNow()).toBeGreaterThanOrEqual(T0 + 45_000);
    // formatListTime's 刚刚 → 1m boundary (45s) is now visible.
    expect(getInboxClockNow() - T0).toBeGreaterThanOrEqual(45_000);
    off();

    // Once unsubscribed the display timer stops (no leaked ticks).
    const countAfter = ticks.length;
    vi.advanceTimersByTime(60_000);
    expect(ticks).toHaveLength(countAfter);
  });

  it("keeps deadline invalidation while the display tick runs", () => {
    const ticks: number[] = [];
    const off = subscribeInboxClock(() => ticks.push(getInboxClockNow()));
    syncInboxDeadlineClock([card(new Date(T0 + 5_000).toISOString())], T0);

    // Deadline crosses first (5s), with no display tick due yet.
    vi.advanceTimersByTime(5_002);
    const deadlineTicks = ticks.length;
    expect(ticks[deadlineTicks - 1]).toBeGreaterThanOrEqual(T0 + 5_000);
    expect(deadlineTicks).toBe(1);

    // Display ticks continue afterwards on their own.
    vi.advanceTimersByTime(45_000);
    expect(ticks.length).toBeGreaterThan(deadlineTicks);
    off();
  });
});
