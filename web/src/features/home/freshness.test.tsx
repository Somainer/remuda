import { act, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { setClock } from "../../lib/clock";
import { HOME_FRESHNESS, useHomeFreshness } from "./freshness";
import { HubBanner } from "./HubBanner";

/**
 * home.md §8 新鲜度, asserted on the injected clock and fake timers: the
 * 5 s banner debounce and the 10 s stale window are product constants, so no
 * test sleeps on wall-clock milliseconds.
 */

type Health = { lastOkAt: number | null; failingSince: number | null; settledSeq: number };

const fake = vi.hoisted(() => ({
  health: { lastOkAt: null, failingSince: null, settledSeq: 0 } as {
    lastOkAt: number | null;
    failingSince: number | null;
    settledSeq: number;
  },
  connection: "live",
  storeListeners: new Set<() => void>(),
  healthListeners: new Set<() => void>(),
  refresh: vi.fn(async () => undefined),
}));

vi.mock("../../lib/store", () => ({
  useHub: () => ({ connection: fake.connection }),
  hubStore: {
    getRefreshHealth: () => fake.health,
    subscribeRefreshHealth: (listener: () => void) => {
      fake.healthListeners.add(listener);
      return () => fake.healthListeners.delete(listener);
    },
    getSnapshot: () => ({ connection: fake.connection }),
    subscribe: (listener: () => void) => {
      fake.storeListeners.add(listener);
      return () => fake.storeListeners.delete(listener);
    },
    refresh: () => fake.refresh(),
  },
}));

let clock = 0;
const restoreClock = () => setClock(null);

/** Advance the injected clock and the fake timers together. */
async function advance(ms: number) {
  clock += ms;
  await act(async () => {
    await vi.advanceTimersByTimeAsync(ms);
  });
}

/** Publish a store refresh outcome (the real store's settle notification). */
function settle(patch: Partial<Health>, bumpSeq = true) {
  fake.health = {
    ...fake.health,
    ...patch,
    settledSeq: fake.health.settledSeq + (bumpSeq ? 1 : 0),
  };
  act(() => {
    for (const listener of fake.healthListeners) listener();
  });
}

/** The page returns to the foreground (PhoneShell/Shell do the refresh). */
function foreground() {
  act(() => {
    document.dispatchEvent(new Event("visibilitychange"));
  });
}

function Probe() {
  const freshness = useHomeFreshness();
  return (
    <>
      <span data-testid="state">{freshness.state}</span>
      <HubBanner />
    </>
  );
}

function state(): string {
  return screen.getByTestId("state").textContent ?? "";
}

beforeEach(() => {
  vi.useFakeTimers();
  clock = 100_000;
  setClock(() => clock);
  fake.health = { lastOkAt: null, failingSince: null, settledSeq: 0 };
  fake.connection = "live";
  fake.refresh.mockClear();
  fake.healthListeners.clear();
  fake.storeListeners.clear();
});

afterEach(() => {
  vi.useRealTimers();
  restoreClock();
});

describe("home freshness", () => {
  it("exports the §8 constants in one place", () => {
    expect(HOME_FRESHNESS).toEqual({
      pollMs: 2_000,
      staleAfterMs: 10_000,
      bannerAfterMs: 5_000,
      receiptMs: 2_000,
    });
  });

  it("is unknown before the first snapshot, even while failures accrue", async () => {
    render(<Probe />);
    expect(state()).toBe("unknown");
    // A first failure run does not invent a snapshot.
    settle({ failingSince: clock });
    expect(state()).toBe("unknown");
    await advance(HOME_FRESHNESS.bannerAfterMs);
    // ...but the banner can still say the Hub is unreachable.
    expect(screen.getByTestId("hub-banner")).toHaveTextContent("上次更新 –");
  });

  it("is fresh while live and recently read, and stale once the 10 s window passes", async () => {
    fake.health = { lastOkAt: clock, failingSince: null, settledSeq: 1 };
    render(<Probe />);
    expect(state()).toBe("fresh");
    // The window is inclusive: exactly 10 s old is still fresh.
    await advance(HOME_FRESHNESS.staleAfterMs);
    expect(state()).toBe("fresh");
    // The next 1 s clock tick re-evaluates past the threshold.
    await advance(1_000);
    expect(state()).toBe("stale");
  });

  it("a 4 s disconnect that recovers never shows the banner", async () => {
    fake.health = { lastOkAt: clock, failingSince: null, settledSeq: 1 };
    render(<Probe />);
    settle({ failingSince: clock });
    await advance(4_000);
    expect(screen.queryByTestId("hub-banner")).toBeNull();
    // Recovery: a successful refresh clears the failing run before 5 s.
    settle({ lastOkAt: clock, failingSince: null });
    await advance(2_000);
    expect(screen.queryByTestId("hub-banner")).toBeNull();
  });

  it("5 s of consecutive failures show the banner; 重试 refreshes once; a success hides it", async () => {
    fake.health = { lastOkAt: clock, failingSince: null, settledSeq: 1 };
    render(<Probe />);
    settle({ failingSince: clock });
    await advance(HOME_FRESHNESS.bannerAfterMs - 1);
    expect(screen.queryByTestId("hub-banner")).toBeNull();
    await advance(1);
    const banner = screen.getByTestId("hub-banner");
    expect(banner).toHaveTextContent("连不上 Hub");
    expect(banner).toHaveTextContent("上次更新 5 秒前");
    fireEvent.click(screen.getByTestId("hub-banner-retry"));
    expect(fake.refresh).toHaveBeenCalledTimes(1);
    // Any successful refresh resets the timer and hides the banner.
    settle({ lastOkAt: clock, failingSince: null });
    expect(screen.queryByTestId("hub-banner")).toBeNull();
  });

  it("the live region announces the last-update age once a minute, not every second", async () => {
    fake.health = { lastOkAt: clock - 5_000, failingSince: null, settledSeq: 1 };
    render(<Probe />);
    settle({ failingSince: clock });
    await advance(HOME_FRESHNESS.bannerAfterMs);
    const status = screen.getByRole("status");
    expect(status).toHaveTextContent("连不上 Hub");
    // 10 s behind at this point — the same information the visual text shows,
    // at the coarse granularity the announcement uses.
    expect(status).toHaveTextContent("上次更新 0 分钟前");
    await advance(49_000);
    expect(status).toHaveTextContent("上次更新 0 分钟前");
    await advance(1_000);
    expect(status).toHaveTextContent("上次更新 1 分钟前");
  });

  it("a foreground grace is not stale until the first refresh after it settles", async () => {
    // Cached rows from 20 s ago: stale on the open.
    fake.health = { lastOkAt: clock - 20_000, failingSince: null, settledSeq: 1 };
    render(<Probe />);
    expect(state()).toBe("stale");
    foreground();
    // Not stale while the foreground refresh is in flight — even though
    // nothing has succeeded yet.
    expect(state()).toBe("grace");
    await advance(2_000);
    expect(state()).toBe("grace");
    // The first settle ends the grace whether it succeeded...
    settle({ lastOkAt: clock, failingSince: null });
    expect(state()).toBe("fresh");
  });

  it("the grace also ends on a failed refresh, and then the list is stale again", async () => {
    fake.health = { lastOkAt: clock - 20_000, failingSince: null, settledSeq: 1 };
    render(<Probe />);
    foreground();
    expect(state()).toBe("grace");
    settle({ failingSince: clock });
    expect(state()).toBe("stale");
  });
});
