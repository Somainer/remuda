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

function card(deadlineISO: string): Interaction {
  return {
    id: "int_1",
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

  it("periodic store re-syncs advance the read clock and do not postpone the deadline tick", () => {
    // Reproduces the e2e failure: a 2 s poll re-enters the sync before the
    // deadline. It must neither freeze clockNow nor reschedule the tick past
    // the crossing.
    const ticks: number[] = [];
    const off = subscribeInboxClock(() => ticks.push(getInboxClockNow()));
    syncInboxDeadlineClock([card(new Date(T0 + 100).toISOString())], T0);

    // A poll lands at +50 ms (fresh page, same card, explicit wall time).
    vi.advanceTimersByTime(50);
    syncInboxDeadlineClock([card(new Date(T0 + 100).toISOString())], T0 + 50);
    expect(getInboxClockNow()).toBe(T0 + 50);

    // Another poll at +90 ms.
    vi.advanceTimersByTime(40);
    syncInboxDeadlineClock([card(new Date(T0 + 100).toISOString())], T0 + 90);

    // Crossing: exactly one tick, no later than the deadline + slack.
    vi.advanceTimersByTime(15);
    expect(ticks).toHaveLength(1);
    expect(ticks[0]!).toBeGreaterThanOrEqual(T0 + 100);
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
